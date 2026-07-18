use std::ops::ControlFlow;

use flume::Sender;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{FastPricing, Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse,
    ThinkingConfig, TokenUsage,
};

pub(super) const BETA_TOOL_EXAMPLES_BEDROCK: &str = "tool-examples-2025-10-29";

/// The messages API refuses requests without max_tokens. Anthropic-kind
/// models always get a window from the fallback table, so this only fires if
/// an unknown-window model is ever routed here; 32k is safe for every Claude.
pub(crate) const FALLBACK_MAX_TOKENS: u32 = 32_000;

/// A `-1m` suffix is our own convention for asking Anthropic for the 1M context
/// window. We strip it from the id before sending and add [`LONG_CONTEXT_BETA`]
/// to the request instead.
pub(crate) const LONG_CONTEXT_SUFFIX: &str = "-1m";
pub(crate) const LONG_CONTEXT_BETA: &str = "context-1m-2025-08-07";
pub(crate) const LONG_CONTEXT_WINDOW: u32 = 1_000_000;

pub(crate) fn strip_long_context(model_id: &str) -> &str {
    model_id
        .strip_suffix(LONG_CONTEXT_SUFFIX)
        .unwrap_or(model_id)
}

/// A `-1m` model is just its base entry with a wider window.
pub(crate) fn long_context_window(model_id: &str) -> Option<u32> {
    model_id
        .ends_with(LONG_CONTEXT_SUFFIX)
        .then_some(LONG_CONTEXT_WINDOW)
}

pub(super) const MESSAGE_CACHE_BREAKPOINTS: usize = 2;

#[derive(Serialize)]
pub(crate) struct CacheControl {
    pub r#type: &'static str,
}

pub(crate) const EPHEMERAL: CacheControl = CacheControl {
    r#type: "ephemeral",
};

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
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
struct MessagePayload {
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct MessageStartEvent {
    message: MessagePayload,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseContentBlock {
    Text,
    Thinking,
    RedactedThinking { data: String },
    ToolUse { id: String, name: String },
}

#[derive(Deserialize)]
struct ContentBlockStartEvent {
    index: usize,
    content_block: SseContentBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Delta {
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
struct ContentBlockDeltaEvent {
    index: usize,
    delta: Delta,
}

#[derive(Deserialize)]
struct MessageDeltaPayload {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct MessageDeltaEvent {
    #[serde(default)]
    delta: Option<MessageDeltaPayload>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Serialize)]
pub(crate) struct SystemBlock<'a> {
    pub r#type: &'static str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
pub(super) struct WireContentBlock<'a> {
    #[serde(flatten)]
    pub inner: &'a ContentBlock,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
pub(super) struct WireMessage<'a> {
    pub role: &'a Role,
    pub content: Vec<WireContentBlock<'a>>,
}

pub(super) fn build_wire_messages(messages: &[Message]) -> Vec<WireMessage<'_>> {
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

pub(super) fn build_wire_tools(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    let mut out: Vec<Value> = arr.to_vec();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Value::Array(out)
}

pub(crate) fn build_request_body_with_system(
    model: &Model,
    messages: &[Message],
    system_blocks: &[SystemBlock<'_>],
    tools: &Value,
    thinking: ThinkingConfig,
) -> Value {
    let wire_messages = build_wire_messages(messages);
    let wire_tools = build_wire_tools(tools);

    let mut body = json!({
        "max_tokens": model.max_output_tokens.unwrap_or(FALLBACK_MAX_TOKENS),
        "system": system_blocks,
        "messages": wire_messages,
        "tools": wire_tools,
    });

    thinking.apply_to_body(&mut body, model);
    body
}

pub(super) struct EventParser {
    content_blocks: Vec<ContentBlock>,
    current_tool_json: String,
    current_block_idx: usize,
    usage: TokenUsage,
    stop_reason: Option<StopReason>,
}

impl EventParser {
    pub fn new() -> Self {
        Self {
            content_blocks: Vec::new(),
            current_tool_json: String::new(),
            current_block_idx: 0,
            usage: TokenUsage::default(),
            stop_reason: None,
        }
    }

    pub async fn process(
        &mut self,
        event_type: &str,
        data: &str,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<ControlFlow<(), ()>, AgentError> {
        match event_type {
            "message_start" => {
                if let Ok(ev) = serde_json::from_str::<MessageStartEvent>(data)
                    && let Some(u) = ev.message.usage
                {
                    self.usage = TokenUsage::from(u);
                }
            }
            "content_block_start" => match serde_json::from_str::<ContentBlockStartEvent>(data) {
                Ok(ev) => {
                    self.current_block_idx = ev.index;
                    match ev.content_block {
                        SseContentBlock::Text => {
                            self.content_blocks.push(ContentBlock::Text {
                                text: String::new(),
                            });
                        }
                        SseContentBlock::Thinking => {
                            self.content_blocks.push(ContentBlock::Thinking {
                                thinking: String::new(),
                                signature: None,
                            });
                        }
                        SseContentBlock::RedactedThinking { data } => {
                            self.content_blocks
                                .push(ContentBlock::RedactedThinking { data });
                        }
                        SseContentBlock::ToolUse { id, name } => {
                            self.current_tool_json.clear();
                            event_tx
                                .send_async(ProviderEvent::ToolUseStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                })
                                .await?;
                            self.content_blocks.push(ContentBlock::ToolUse {
                                id,
                                name,
                                input: Value::Null,
                            });
                        }
                    }
                }
                Err(e) => warn!(error = %e, "failed to parse content_block_start"),
            },
            "content_block_delta" => match serde_json::from_str::<ContentBlockDeltaEvent>(data) {
                Ok(ev) => {
                    self.current_block_idx = ev.index;
                    let block = self.content_blocks.get_mut(self.current_block_idx);
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
                            self.current_tool_json.push_str(&partial_json);
                        }
                    }
                }
                Err(e) => warn!(error = %e, "failed to parse content_block_delta"),
            },
            "content_block_stop" => {
                if let Some(ContentBlock::ToolUse { name, input, .. }) =
                    self.content_blocks.get_mut(self.current_block_idx)
                {
                    *input = match serde_json::from_str(&self.current_tool_json) {
                        Ok(v) => {
                            debug!(tool = %name, json = %self.current_tool_json, "tool input JSON");
                            v
                        }
                        Err(e) => {
                            warn!(error = %e, json = %self.current_tool_json, "malformed tool JSON, falling back to {{}}");
                            Value::Object(Default::default())
                        }
                    };
                    self.current_tool_json.clear();
                }
            }
            "message_delta" => {
                if let Ok(ev) = serde_json::from_str::<MessageDeltaEvent>(data) {
                    if let Some(u) = ev.usage {
                        self.usage.output = u.output_tokens;
                    }
                    if let Some(d) = ev.delta {
                        self.stop_reason = d
                            .stop_reason
                            .map(|s| StopReason::from_anthropic(&s))
                            .or(self.stop_reason.take());
                    }
                }
            }
            "error" => {
                if let Ok(ev) = serde_json::from_str::<super::super::SseErrorPayload>(data) {
                    warn!(error_type = %ev.error.r#type, message = %ev.error.message, "SSE error event");
                    return Err(ev.into_agent_error());
                }
                warn!(raw = %data, "unparseable SSE error event");
                return Err(AgentError::Api {
                    status: 400,
                    message: data.to_string(),
                });
            }
            "message_stop" => return Ok(ControlFlow::Break(())),
            _ => {}
        }

        Ok(ControlFlow::Continue(()))
    }

    pub fn finish(self) -> StreamResponse {
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

pub(crate) fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["claude-haiku-4-5"],
            tier: ModelTier::Weak,
            family: ModelFamily::Claude,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.00,
                output: 5.00,
                cache_write: 1.25,
                cache_read: 0.10,
                fast: None,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4-6"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-sonnet-4"],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 3.00,
                output: 15.00,
                cache_write: 3.75,
                cache_read: 0.30,
                fast: None,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: None,
            },
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-6"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: Some(FastPricing {
                    input: 30.00,
                    output: 150.00,
                }),
            },
            max_output_tokens: 128000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-7"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: Some(FastPricing {
                    input: 30.00,
                    output: 150.00,
                }),
            },
            max_output_tokens: 128000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-8"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 5.00,
                output: 25.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: Some(FastPricing {
                    input: 10.00,
                    output: 50.00,
                }),
            },
            max_output_tokens: 128000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-fable-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 10.00,
                output: 50.00,
                cache_write: 12.50,
                cache_read: 1.00,
                fast: None,
            },
            max_output_tokens: 128000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4-0", "claude-opus-4-1"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 15.00,
                output: 75.00,
                cache_write: 18.75,
                cache_read: 1.50,
                fast: None,
            },
            max_output_tokens: 32000,
            context_window: 200_000,
        },
    ];
    MODELS
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{
        LONG_CONTEXT_SUFFIX, LONG_CONTEXT_WINDOW, long_context_window, strip_long_context,
    };

    #[test_case("claude-opus-4-8-1m", "claude-opus-4-8" ; "strips_suffix")]
    #[test_case("claude-opus-4-8", "claude-opus-4-8" ; "leaves_plain_id")]
    fn strip_long_context_removes_suffix(model_id: &str, expected: &str) {
        assert_eq!(strip_long_context(model_id), expected);
    }

    #[test_case("claude-opus-4-8-1m", Some(LONG_CONTEXT_WINDOW) ; "suffix_opts_in")]
    #[test_case("claude-opus-4-8", None ; "plain_id_keeps_base")]
    fn long_context_window_follows_suffix(model_id: &str, expected: Option<u32>) {
        assert_eq!(long_context_window(model_id), expected);
        assert!(LONG_CONTEXT_SUFFIX.ends_with("1m"));
    }
}
