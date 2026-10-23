use std::collections::HashSet;
use std::ops::ControlFlow;
use std::sync::LazyLock;

use flume::Sender;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::Model;
use crate::providers::copilot::copilot_usd;
use crate::types::{is_deferred_tool, rejects_sampling};
use crate::{
    AgentError, ContentBlock, EMPTY_RESPONSE_MARKER, InputTransformation, Message, ProviderEvent,
    Role, StopReason, StreamResponse, ThinkingConfig, TokenUsage,
};

pub(super) const BETA_TOOL_EXAMPLES_BEDROCK: &str = "tool-examples-2025-10-29";
/// Unlocks `defer_loading` and `tool_reference` expansion. The direct API
/// no longer requires it, but gateways that pin an older surface still do.
pub(super) const BETA_DEFERRED_TOOLS: &str = "advanced-tool-use-2025-11-20";
pub(super) const BETA_DEFERRED_TOOLS_BEDROCK: &str = "tool-search-tool-2025-10-19";

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

pub(crate) const DISPLACED_ERROR_PREFIX: &str = "Tool call failed: ";

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

static EMPTY_CONTENT: LazyLock<ContentBlock> = LazyLock::new(|| ContentBlock::Text {
    text: EMPTY_RESPONSE_MARKER.into(),
});

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
            ..Default::default()
        }
    }
}

/// Skips entries of a shape this build doesn't know. The field is in beta, and
/// failing the whole event over it would lose its usage and stop reason too.
fn lenient_transformations<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<InputTransformation>>, D::Error> {
    let Value::Array(entries) = Value::deserialize(deserializer)? else {
        return Ok(None);
    };
    let parsed = entries
        .into_iter()
        .filter_map(|entry| {
            InputTransformation::deserialize(&entry)
                .inspect_err(
                    |e| warn!(error = %e, %entry, "skipping unparseable input transformation"),
                )
                .ok()
        })
        .collect();
    Ok(Some(parsed))
}

#[derive(Deserialize)]
struct MessagePayload {
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default, deserialize_with = "lenient_transformations")]
    input_transformations: Option<Vec<InputTransformation>>,
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
    #[serde(default)]
    copilot_usage: Option<Value>,
    /// Only after a server-side fallback, with the serving model's entries.
    #[serde(default, deserialize_with = "lenient_transformations")]
    input_transformations: Option<Vec<InputTransformation>>,
}

#[derive(Serialize)]
pub(crate) struct SystemBlock<'a> {
    pub r#type: &'static str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
pub(crate) struct WireContentBlock<'a> {
    #[serde(flatten)]
    pub inner: WireBlock<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Only a tool result that loaded tools is rebuilt: `loaded_tools` is not a
/// wire field, and the loads become `tool_reference` parts.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum WireBlock<'a> {
    Verbatim(&'a ContentBlock),
    Rebuilt(Value),
}

impl WireBlock<'_> {
    fn is_thinking(&self) -> bool {
        matches!(self, Self::Verbatim(block) if block.is_thinking())
    }
}

fn deferred_tool_names(tools: &Value) -> HashSet<&str> {
    tools
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| is_deferred_tool(t))
        .filter_map(|t| t["name"].as_str())
        .collect()
}

/// Only a name the request still defers may be referenced: the API rejects
/// a reference to a name it was not handed, so a load recorded while a
/// server was up degrades to text once that server is gone. And it refuses
/// a `tool_result` mixing references with anything else, so the text is
/// displaced to a sibling block. An errored result may hold only text, and
/// the retry needs the definition more than the flag, so the error rides
/// along as a prefix on the displaced text.
///
/// Each name is referenced once per request, by the first result that loaded
/// it. Every later call records the load again so compaction cannot drop the
/// only reference, but referencing it each time would pull every later
/// output out of its own result, leaving the model to pair them by order.
fn wire_block<'a>(
    block: &'a ContentBlock,
    unreferenced: &mut HashSet<&str>,
) -> (WireBlock<'a>, Option<Value>) {
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
        loaded_tools,
    } = block
    else {
        return (WireBlock::Verbatim(block), None);
    };
    if loaded_tools.is_empty() {
        return (WireBlock::Verbatim(block), None);
    }
    let text = if content.trim().is_empty() {
        EMPTY_RESPONSE_MARKER
    } else {
        content
    };
    let refs: Vec<Value> = loaded_tools
        .iter()
        .filter(|name| unreferenced.remove(name.as_str()))
        .map(|name| json!({"type": "tool_reference", "tool_name": name}))
        .collect();
    let mut result = json!({"type": "tool_result", "tool_use_id": tool_use_id});
    if refs.is_empty() {
        result["content"] = json!([text_block(text)]);
        if *is_error {
            result["is_error"] = json!(true);
        }
        return (WireBlock::Rebuilt(result), None);
    }
    result["content"] = Value::Array(refs);
    let displaced = if *is_error {
        text_block(&format!("{DISPLACED_ERROR_PREFIX}{text}"))
    } else {
        text_block(text)
    };
    (WireBlock::Rebuilt(result), Some(displaced))
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

#[derive(Serialize)]
pub(crate) struct WireMessage<'a> {
    pub role: &'a Role,
    pub content: Vec<WireContentBlock<'a>>,
}

/// The API rejects blank text blocks and thinking it did not sign (e.g. an
/// OpenAI reasoning summary from earlier in the session). A block this build
/// does not know is never sent.
fn is_replayable(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text { text } => !text.trim().is_empty(),
        ContentBlock::Thinking { signature, .. } => signature.is_some(),
        ContentBlock::Unknown(_) => false,
        _ => true,
    }
}

/// A message left with no block at all is rejected too, so it falls back to
/// the marker. Displaced texts go after every result, since the API wants
/// all `tool_result` blocks first.
fn wire_content<'a>(
    msg: &'a Message,
    unreferenced: &mut HashSet<&str>,
) -> Vec<WireContentBlock<'a>> {
    let plain = |inner| WireContentBlock {
        inner,
        cache_control: None,
    };
    let mut displaced = Vec::new();
    let mut content: Vec<WireContentBlock<'a>> = msg
        .content
        .iter()
        .filter(|block| is_replayable(block))
        .map(|block| {
            let (inner, sibling) = wire_block(block, unreferenced);
            displaced.extend(sibling);
            plain(inner)
        })
        .collect();
    content.extend(
        displaced
            .into_iter()
            .map(|block| plain(WireBlock::Rebuilt(block))),
    );

    if content.is_empty() {
        content.push(plain(WireBlock::Verbatim(&EMPTY_CONTENT)));
    }
    content
}

/// The single Anthropic-protocol message encoder; every provider speaking
/// that protocol must go through it. `tools` must be the request's own
/// array: it decides which recorded loads may replay as references.
pub(crate) fn wire_messages<'a>(messages: &'a [Message], tools: &Value) -> Vec<WireMessage<'a>> {
    let mut unreferenced = deferred_tool_names(tools);
    messages
        .iter()
        .map(|msg| WireMessage {
            role: &msg.role,
            content: wire_content(msg, &mut unreferenced),
        })
        .collect()
}

pub(super) fn build_wire_messages<'a>(
    messages: &'a [Message],
    tools: &Value,
) -> Vec<WireMessage<'a>> {
    let mut wire = wire_messages(messages, tools);
    let first_cached = wire.len().saturating_sub(MESSAGE_CACHE_BREAKPOINTS);

    // The API rejects `cache_control` on thinking blocks, so walk back to
    // the last block that can carry it. All thinking means no breakpoint,
    // which beats a fatal one.
    for msg in &mut wire[first_cached..] {
        if let Some(block) = msg.content.iter_mut().rfind(|b| !b.inner.is_thinking()) {
            block.cache_control = Some(EPHEMERAL);
        }
    }
    wire
}

/// A deferred definition is not in the cached prefix and may not carry a
/// breakpoint, so it goes on the last one that is.
pub(super) fn build_wire_tools(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    let mut out: Vec<Value> = arr.to_vec();
    if let Some(last) = out.iter_mut().rev().find(|t| !is_deferred_tool(t)) {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Value::Array(out)
}

pub(super) fn has_deferred_tools(tools: &Value) -> bool {
    tools
        .as_array()
        .is_some_and(|arr| arr.iter().any(is_deferred_tool))
}

pub(crate) fn build_request_body_with_system(
    model: &Model,
    messages: &[Message],
    system_blocks: &[SystemBlock<'_>],
    tools: &Value,
    thinking: ThinkingConfig,
    top_p: Option<f64>,
) -> Value {
    let wire_messages = build_wire_messages(messages, tools);
    let wire_tools = build_wire_tools(tools);

    let mut body = json!({
        "max_tokens": model.output_tokens().unwrap_or(FALLBACK_MAX_TOKENS),
        "system": system_blocks,
        "messages": wire_messages,
        "tools": wire_tools,
    });
    if let Some(top_p) = top_p
        && !thinking.is_enabled()
        && !rejects_sampling(&model.id)
    {
        body["top_p"] = json!(top_p);
    }

    thinking.apply_to_body(&mut body, model);
    body
}

pub(super) struct EventParser {
    content_blocks: Vec<ContentBlock>,
    current_tool_json: String,
    current_block_idx: usize,
    usage: TokenUsage,
    stop_reason: Option<StopReason>,
    input_transformations: Vec<InputTransformation>,
}

impl EventParser {
    pub fn new() -> Self {
        Self {
            content_blocks: Vec::new(),
            current_tool_json: String::new(),
            current_block_idx: 0,
            usage: TokenUsage::default(),
            stop_reason: None,
            input_transformations: Vec::new(),
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
                if let Ok(ev) = serde_json::from_str::<MessageStartEvent>(data) {
                    if let Some(u) = ev.message.usage {
                        self.usage = TokenUsage::from(u);
                    }
                    if let Some(t) = ev.message.input_transformations {
                        self.input_transformations = t;
                    }
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
                            self.content_blocks
                                .push(ContentBlock::tool_use(id, name, Value::Null));
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
                        // Gateways like Bifrost send zeros in message_start and the real
                        // counts here. The first-party API often leaves these fields out, and
                        // serde turns a missing field into 0, so a zero must not wipe what
                        // message_start gave us.
                        for (dst, src) in [
                            (&mut self.usage.input, u.input_tokens),
                            (&mut self.usage.cache_read, u.cache_read_input_tokens),
                            (
                                &mut self.usage.cache_creation,
                                u.cache_creation_input_tokens,
                            ),
                        ] {
                            if src > 0 {
                                *dst = src;
                            }
                        }
                    }
                    if let Some(usd) = ev.copilot_usage.as_ref().and_then(copilot_usd) {
                        self.usage.cost = Some(usd);
                    }
                    if let Some(t) = ev.input_transformations {
                        self.input_transformations = t;
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
                return Err(AgentError::api(400, data.to_string()));
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
            input_transformations: self.input_transformations,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        LONG_CONTEXT_SUFFIX, LONG_CONTEXT_WINDOW, SystemBlock, build_request_body_with_system,
        long_context_window, strip_long_context,
    };
    use crate::model::{Model, ModelFamily, ModelPricing, ModelTier};
    use crate::{ContentBlock, Message, Role, ThinkingConfig};

    const SYSTEM: &str = "sys";
    const TOOL_USE_ID: &str = "toolu_1";
    const TOOL_SEARCH: &str = "tool_search";
    const LOADED_TOOL: &str = "srv__fetch";

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

    fn test_model() -> Model {
        Model {
            id: "claude-test".into(),
            provider: Arc::<str>::from("anthropic"),
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            supports_fast_override: None,
            pricing: ModelPricing::default(),
            subsidised_by: None,
            discovered_free: false,
            max_output_tokens: Some(8192),
            turn_output_tokens: None,
            context_window: 200_000,
            thinking_fields: None,
        }
    }

    #[test_case("claude-test", ThinkingConfig::Off, true ; "off_sends_top_p")]
    #[test_case("claude-test", ThinkingConfig::Adaptive, false ; "thinking_omits_top_p")]
    #[test_case("claude-opus-4-7", ThinkingConfig::Off, false ; "adaptive_only_model_omits_top_p")]
    fn top_p_is_sent_unless_thinking_or_adaptive_only(
        model_id: &str,
        thinking: ThinkingConfig,
        sent: bool,
    ) {
        let model = Model {
            id: model_id.into(),
            ..test_model()
        };
        let body = build_request_body_with_system(
            &model,
            &[Message::user("hi".into())],
            &[SystemBlock {
                r#type: "text",
                text: "sys",
                cache_control: None,
            }],
            &json!([]),
            thinking,
            Some(0.8),
        );
        assert_eq!(body.get("top_p") == Some(&json!(0.8)), sent);
    }

    fn strip_cache_control(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("cache_control");
                map.values_mut().for_each(strip_cache_control);
            }
            Value::Array(items) => items.iter_mut().for_each(strip_cache_control),
            _ => {}
        }
    }

    fn encode(messages: &[Message], tools: &Value) -> Value {
        let mut body = build_request_body_with_system(
            &test_model(),
            messages,
            &[SystemBlock {
                r#type: "text",
                text: SYSTEM,
                cache_control: None,
            }],
            tools,
            ThinkingConfig::Adaptive,
            None,
        );
        strip_cache_control(&mut body);
        body
    }

    /// Cache breakpoints move every turn and the cache ignores them. Everything
    /// else must come back unchanged at the front of the next request, signed
    /// thinking and replayed tool loads included, even after a late tool joined.
    #[test]
    fn encoded_request_is_a_prefix_of_the_next() {
        let deferred = |name: &str| json!({"name": name, "defer_loading": true});
        let tools = json!([{"name": "read"}, deferred(LOADED_TOOL)]);
        let mut messages = vec![
            Message::user("go".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "plan".into(),
                        signature: Some("sig".into()),
                    },
                    ContentBlock::tool_use(TOOL_USE_ID, TOOL_SEARCH, json!({})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: TOOL_USE_ID.into(),
                    content: String::new(),
                    is_error: false,
                    loaded_tools: vec![LOADED_TOOL.into()],
                }],
                ..Default::default()
            },
        ];
        let before = encode(&messages, &tools);

        let mut grown_tools = tools.clone();
        grown_tools.as_array_mut().unwrap().push(deferred("late"));
        messages.push(Message::context_update("date".into(), Default::default()));
        messages.push(Message::user("more".into()));
        let after = encode(&messages, &grown_tools);

        assert_eq!(before["system"], after["system"]);
        for key in ["tools", "messages"] {
            let (before, after) = (
                before[key].as_array().unwrap(),
                after[key].as_array().unwrap(),
            );
            assert_eq!(&after[..before.len()], before.as_slice(), "{key}");
        }
    }
}
