//! Anthropic API provider with prompt caching.
//! Cache breakpoints: 1 on tools (last element), 1 on system prompt, 2 on the last two messages
//! (last content block each). This consumes all 4 of Anthropic's allowed breakpoints.

use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::Model;
use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse,
    ThinkingConfig, TokenUsage,
};

const API_VERSION: &str = "2023-06-01";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models?limit=1000";
const BETA_ADVANCED_TOOL_USE: &str = "advanced-tool-use-2025-11-20";

/// Anthropic caches conversation by blocks (tools -> system -> messages).
/// We use 1 cache breakpoint for the last tool block, and 1 for the system prompt.
/// We're allowed to set up to 4 breakpoints. So we're left with 2 to use for messages.
///
/// See https://platform.claude.com/docs/en/build-with-claude/prompt-caching.
const MESSAGE_CACHE_BREAKPOINTS: usize = 2;

#[derive(Serialize)]
struct CacheControl {
    r#type: &'static str,
}

const EPHEMERAL: CacheControl = CacheControl {
    r#type: "ephemeral",
};

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["claude-3-haiku"],
            tier: ModelTier::Weak,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 0.25,
                output: 1.25,
                cache_write: 0.30,
                cache_read: 0.03,
            },
            max_output_tokens: 4096,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-3-5-haiku"],
            tier: ModelTier::Weak,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 0.80,
                output: 4.00,
                cache_write: 1.00,
                cache_read: 0.08,
            },
            max_output_tokens: 8192,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-haiku-4-5"],
            tier: ModelTier::Weak,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing {
                input: 1.00,
                output: 5.00,
                cache_write: 1.25,
                cache_read: 0.10,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-3-sonnet"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 0.30,
                cache_read: 0.30,
            },
            max_output_tokens: 4096,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-3-5-sonnet"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
            },
            max_output_tokens: 8192,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-3-7-sonnet", "claude-sonnet-4"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-6-1m"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
            },
            max_output_tokens: 64000,
            context_window: 1_000_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-6"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-7-1m"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
            },
            max_output_tokens: 128000,
            context_window: 1_000_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-7"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
            },
            max_output_tokens: 128000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-6-1m"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
            },
            max_output_tokens: 128000,
            context_window: 1_000_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-6"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
            },
            max_output_tokens: 128000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-3-opus", "claude-opus-4-0", "claude-opus-4-1"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: false,
            pricing: ModelPricing {
                input: 15.00,
                output: 75.00,
                cache_write: 18.75,
                cache_read: 1.50,
            },
            max_output_tokens: 32000,
            context_window: 200_000,
        },
    ]
}

fn resolve_auth() -> Result<super::ResolvedAuth, AgentError> {
    if let Ok(key) = env::var("ANTHROPIC_API_KEY") {
        debug!("using API key authentication");
        return Ok(super::ResolvedAuth {
            base_url: Some("https://api.anthropic.com/v1/messages".into()),
            headers: vec![
                ("x-api-key".into(), key),
                ("anthropic-beta".into(), BETA_ADVANCED_TOOL_USE.into()),
            ],
        });
    }

    Err(AgentError::Config {
        message: "set ANTHROPIC_API_KEY environment variable".into(),
    })
}

#[derive(Deserialize)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) input_tokens: u32,
    #[serde(default)]
    pub(crate) output_tokens: u32,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: u32,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: u32,
}

impl From<Usage> for TokenUsage {
    fn from(u: Usage) -> Self {
        Self {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_creation: u.cache_creation_input_tokens,
            cache_read: u.cache_read_input_tokens,
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct MessagePayload {
    #[serde(default)]
    pub(crate) usage: Option<Usage>,
}

#[derive(Deserialize)]
pub(crate) struct MessageStartEvent {
    pub(crate) message: MessagePayload,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SseContentBlock {
    Text,
    Thinking,
    RedactedThinking { data: String },
    ToolUse { id: String, name: String },
}

#[derive(Deserialize)]
pub(crate) struct ContentBlockStartEvent {
    pub(crate) index: usize,
    pub(crate) content_block: SseContentBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
}

#[derive(Deserialize)]
pub(crate) struct ContentBlockDeltaEvent {
    pub(crate) index: usize,
    pub(crate) delta: Delta,
}

#[derive(Deserialize)]
pub(crate) struct MessageDeltaPayload {
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct MessageDeltaEvent {
    #[serde(default)]
    pub(crate) delta: Option<MessageDeltaPayload>,
    #[serde(default)]
    pub(crate) usage: Option<Usage>,
}

pub struct Anthropic {
    client: HttpClient,
    auth: Arc<Mutex<super::ResolvedAuth>>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
}

impl Anthropic {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let resolved = resolve_auth()?;
        Ok(Self {
            client: super::http_client(timeouts),
            auth: Arc::new(Mutex::new(resolved)),
            system_prefix: None,
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Self {
        Self {
            client: super::http_client(timeouts),
            auth,
            system_prefix: None,
            stream_timeout: timeouts.stream,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn build_request(&self, method: &str, url: Option<&str>) -> isahc::http::request::Builder {
        let auth = self.auth.lock().unwrap();
        let url = url.unwrap_or_else(|| auth.base_url.as_deref().unwrap_or(MESSAGES_URL));
        let mut builder = Request::builder()
            .method(method)
            .uri(url)
            .header("anthropic-version", API_VERSION);
        for (key, value) in &auth.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder
    }

    async fn do_stream_request(
        &self,
        body: &Value,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<StreamResponse, AgentError> {
        let json_body = serde_json::to_vec(body)?;
        let request = self
            .build_request("POST", None)
            .header("content-type", "application/json")
            .body(json_body)?;
        let response = self.client.send_async(request).await?;
        let status = response.status().as_u16();

        if status == 200 {
            parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn do_list_models(&self) -> Result<Vec<String>, AgentError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            let mut url = MODELS_URL.to_string();
            if let Some(cursor) = &after_id {
                url.push_str(&format!("&after_id={cursor}"));
            }

            let request = self.build_request("GET", Some(&url)).body(())?;
            let mut response = self.client.send_async(request).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }

            let body_text = response.text().await?;
            let page: ModelsPage = serde_json::from_str(&body_text)?;
            models.extend(page.data.into_iter().map(|m| m.id));

            if !page.has_more {
                break;
            }
            after_id = page.last_id;
        }

        models.sort();
        Ok(models)
    }
}

impl Provider for Anthropic {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        thinking: ThinkingConfig,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let wire_messages = build_wire_messages(messages);
            let wire_tools = build_wire_tools(tools);
            let system_block = SystemBlock {
                r#type: "text",
                text: system,
                cache_control: Some(EPHEMERAL),
            };

            let system_blocks = if let Some(prefix) = &self.system_prefix {
                let prefix_block = SystemBlock {
                    r#type: "text",
                    text: prefix,
                    cache_control: None,
                };
                json!([prefix_block, system_block])
            } else {
                json!([system_block])
            };

            let mut body = json!({
                "model": model.id,
                "max_tokens": model.max_output_tokens,
                "system": system_blocks,
                "messages": wire_messages,
                "tools": wire_tools,
                "stream": true,
            });

            thinking.apply_to_body(&mut body);

            debug!(model = %model.id, num_messages = messages.len(), ?thinking, "sending API request");
            self.do_stream_request(&body, event_tx).await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let resolved = resolve_auth()?;
            *self.auth.lock().unwrap() = resolved;
            debug!("reloaded Anthropic auth from env");
            Ok(())
        })
    }
}

#[derive(Deserialize)]
struct ModelInfo {
    id: String,
}

#[derive(Deserialize)]
struct ModelsPage {
    data: Vec<ModelInfo>,
    has_more: bool,
    last_id: Option<String>,
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    r#type: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct WireContentBlock<'a> {
    #[serde(flatten)]
    inner: &'a ContentBlock,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a Role,
    content: Vec<WireContentBlock<'a>>,
}

fn build_wire_messages(messages: &[Message]) -> Vec<WireMessage<'_>> {
    let len = messages.len();

    messages
        .iter()
        .enumerate()
        .map(|(msg_idx, msg)| {
            let cache_last_block = msg_idx + MESSAGE_CACHE_BREAKPOINTS >= len;

            WireMessage {
                role: &msg.role,
                content: msg
                    .content
                    .iter()
                    .enumerate()
                    .map(|(block_idx, block)| WireContentBlock {
                        inner: block,
                        cache_control: if cache_last_block && block_idx + 1 == msg.content.len() {
                            Some(EPHEMERAL)
                        } else {
                            None
                        },
                    })
                    .collect(),
            }
        })
        .collect()
}

fn build_wire_tools(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    let mut out: Vec<Value> = arr.to_vec();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Value::Array(out)
}

pub(crate) struct EventProcessingState {
    pub content_blocks: Vec<ContentBlock>,
    pub current_tool_json: String,
    pub current_block_idx: usize,
    pub usage: TokenUsage,
    pub stop_reason: Option<StopReason>,
}

impl EventProcessingState {
    pub fn new() -> Self {
        Self {
            content_blocks: Vec::new(),
            current_tool_json: String::new(),
            current_block_idx: 0,
            usage: TokenUsage::default(),
            stop_reason: None,
        }
    }

    pub fn into_stream_response(self) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: self.content_blocks,
                ..Default::default()
            },
            usage: self.usage,
            stop_reason: self.stop_reason,
        }
    }
}

pub(crate) enum EventAction {
    Continue,
    Stop,
}

pub(crate) async fn process_anthropic_event(
    event_type: &str,
    data: Value,
    state: &mut EventProcessingState,
    event_tx: &Sender<ProviderEvent>,
) -> Result<EventAction, AgentError> {
    match event_type {
        "message_start" => {
            if let Ok(ev) = serde_json::from_value::<MessageStartEvent>(data)
                && let Some(u) = ev.message.usage
            {
                state.usage = TokenUsage::from(u);
            }
        }
        "content_block_start" => match serde_json::from_value::<ContentBlockStartEvent>(data) {
            Ok(ev) => {
                state.current_block_idx = ev.index;
                match ev.content_block {
                    SseContentBlock::Text => {
                        state.content_blocks.push(ContentBlock::Text {
                            text: String::new(),
                        });
                    }
                    SseContentBlock::Thinking => {
                        state.content_blocks.push(ContentBlock::Thinking {
                            thinking: String::new(),
                            signature: None,
                        });
                    }
                    SseContentBlock::RedactedThinking { data } => {
                        state
                            .content_blocks
                            .push(ContentBlock::RedactedThinking { data });
                    }
                    SseContentBlock::ToolUse { id, name } => {
                        state.current_tool_json.clear();
                        event_tx
                            .send_async(ProviderEvent::ToolUseStart {
                                id: id.clone(),
                                name: name.clone(),
                            })
                            .await?;
                        state.content_blocks.push(ContentBlock::ToolUse {
                            id,
                            name,
                            input: Value::Null,
                        });
                    }
                }
            }
            Err(e) => warn!(error = %e, "failed to parse content_block_start"),
        },
        "content_block_delta" => match serde_json::from_value::<ContentBlockDeltaEvent>(data) {
            Ok(ev) => {
                state.current_block_idx = ev.index;
                let block = state.content_blocks.get_mut(state.current_block_idx);
                match ev.delta {
                    Delta::Text { text } => {
                        if !text.is_empty() {
                            if let Some(ContentBlock::Text { text: t }) = block {
                                t.push_str(&text);
                            }
                            event_tx
                                .send_async(ProviderEvent::TextDelta { text })
                                .await?;
                        }
                    }
                    Delta::Thinking { thinking } => {
                        if !thinking.is_empty() {
                            if let Some(ContentBlock::Thinking { thinking: t, .. }) = block {
                                t.push_str(&thinking);
                            }
                            event_tx
                                .send_async(ProviderEvent::ThinkingDelta { text: thinking })
                                .await?;
                        }
                    }
                    Delta::Signature { signature } => {
                        if let Some(ContentBlock::Thinking { signature: sig, .. }) = block {
                            *sig = Some(signature);
                        }
                    }
                    Delta::InputJson { partial_json } => {
                        state.current_tool_json.push_str(&partial_json);
                    }
                }
            }
            Err(e) => warn!(error = %e, "failed to parse content_block_delta"),
        },
        "content_block_stop" => {
            if let Some(ContentBlock::ToolUse { name, input, .. }) = state
                .content_blocks
                .get_mut(state.current_block_idx)
            {
                *input = match serde_json::from_str(&state.current_tool_json) {
                    Ok(v) => {
                        debug!(tool = %name, json = %state.current_tool_json, "tool input JSON");
                        v
                    }
                    Err(e) => {
                        warn!(error = %e, json = %state.current_tool_json, "malformed tool JSON, falling back to {{}}");
                        Value::Object(Default::default())
                    }
                };
                state.current_tool_json.clear();
            }
        }
        "message_delta" => {
            if let Ok(ev) = serde_json::from_value::<MessageDeltaEvent>(data) {
                if let Some(u) = ev.usage {
                    state.usage.output = u.output_tokens;
                }
                if let Some(d) = ev.delta {
                    state.stop_reason = d
                        .stop_reason
                        .map(|s| StopReason::from_anthropic(&s))
                        .or(state.stop_reason.take());
                }
            }
        }
        "error" => {
            if let Ok(ev) = serde_json::from_value::<super::SseErrorPayload>(data) {
                warn!(error_type = %ev.error.r#type, message = %ev.error.message, "SSE error event");
                return Err(ev.into_agent_error());
            }
            return Err(AgentError::Api {
                status: 400,
                message: "unparseable error event".into(),
            });
        }
        "message_stop" => return Ok(EventAction::Stop),
        _ => {}
    }
    Ok(EventAction::Continue)
}

pub(crate) async fn parse_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();
    let mut current_event = String::new();
    let mut state = EventProcessingState::new();
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = super::next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        if let Some(event_type) = line.strip_prefix("event: ") {
            current_event = event_type.to_string();
            continue;
        }

        let data_str = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };

        let data: Value = match serde_json::from_str(data_str) {
            Ok(v) => v,
            Err(_) => {
                if current_event == "error" {
                    return Err(AgentError::Api {
                        status: 400,
                        message: data_str.to_string(),
                    });
                }
                continue;
            }
        };

        match process_anthropic_event(&current_event, data, &mut state, event_tx).await? {
            EventAction::Continue => {}
            EventAction::Stop => break,
        }
    }

    Ok(state.into_stream_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);

    fn mock_response(data: &'static [u8]) -> isahc::Response<isahc::AsyncBody> {
        let body = isahc::AsyncBody::from_bytes_static(data);
        isahc::Response::builder().status(200).body(body).unwrap()
    }

    #[test]
    fn parse_sse_text_and_usage() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":8}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert_eq!(
                resp.usage,
                TokenUsage {
                    input: 42,
                    output: 10,
                    cache_creation: 5,
                    cache_read: 8
                }
            );
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
            );
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));

            let mut deltas = Vec::new();
            while let Ok(e) = rx.try_recv() {
                if let ProviderEvent::TextDelta { text: t } = e {
                    deltas.push(t);
                }
            }
            assert_eq!(deltas, vec!["Hello", " world"]);
        })
    }

    #[test]
    fn parse_sse_tool_use() {
        smol::block_on(async {
            let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"bash\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"echo hi\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].0, "tu_1");
            assert_eq!(tools[0].1, "bash");

            let starts: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ToolUseStart { id, name } => Some((id, name)),
                    _ => None,
                })
                .collect();
            assert_eq!(starts, vec![("tu_1".to_string(), "bash".to_string())]);
        })
    }

    #[test]
    fn cache_control_placement() {
        let single = vec![Message::user("only".into())];
        let wire = build_wire_messages(&single);
        let json: Value = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            json[0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );

        let multi = vec![
            Message::user("first".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "second".into(),
                    },
                ],
                ..Default::default()
            },
        ];
        let wire = build_wire_messages(&multi);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert!(json[0]["content"][0].get("cache_control").is_none());
        assert_eq!(
            json[1]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(json[2]["content"][0].get("cache_control").is_none());
        assert_eq!(
            json[2]["content"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn parse_sse_overloaded_error() {
        smol::block_on(async {
            let input = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(mock_response(input), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 529);
                    assert_eq!(message, "Overloaded");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_unparseable_error() {
        smol::block_on(async {
            let input = b"event: error\ndata: not-json\n";
            let (tx, _rx) = flume::unbounded();
            let err = parse_sse(mock_response(input), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap_err();
            match err {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 400);
                    assert_eq!(message, "not-json");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_malformed_tool_json_yields_empty_object() {
        smol::block_on(async {
            let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_2\",\"name\":\"read\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{broken\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "read");
            assert_eq!(*tools[0].2, Value::Object(Default::default()));
        })
    }

    #[test]
    fn parse_sse_thinking_blocks() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" think\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig123\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n";

            let (tx, rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, signature }
                    if thinking == "Let me think" && *signature == Some("sig123".to_string()))
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello")
            );

            let thinking_deltas: Vec<_> = rx
                .drain()
                .filter_map(|e| match e {
                    ProviderEvent::ThinkingDelta { text } => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking_deltas, vec!["Let me", " think"]);
        })
    }

    #[test]
    fn parse_sse_redacted_thinking() {
        smol::block_on(async {
            let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque_data\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n";

            let (tx, _rx) = flume::unbounded();
            let resp = parse_sse(mock_response(sse_data), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::RedactedThinking { data } if data == "opaque_data")
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hi")
            );
        })
    }
}
