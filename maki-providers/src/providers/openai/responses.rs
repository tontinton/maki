use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use isahc::{HttpClient, Request};
use serde_json::{Value, json};
use tracing::{debug, warn};

#[cfg(test)]
use super::routing::ROUTING_HINT_HEADER;
use super::routing::{RoutingState, TURN_STATE_HEADER, is_coding_plan};
use crate::model::Model;
use crate::providers::openai_compat::tool_parameters;
use crate::providers::{ResolvedAuth, sse_error_status};
use crate::types::EffortDialect;
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse,
    ThinkingConfig, TokenUsage, dialect,
};

const RESPONSES_PATH: &str = "/responses";
const FAILED_RESPONSE_STATUS: u16 = 500;
pub(crate) fn build_body(
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
) -> Value {
    let input = convert_input(messages);
    let wire_tools = convert_tools(tools);

    let mut body = json!({
        "model": model.id,
        "instructions": system,
        "input": input,
        "stream": true,
        "store": false,
    });
    if wire_tools.as_array().is_some_and(|a| !a.is_empty()) {
        body["tools"] = wire_tools;
    }
    body
}

pub(crate) fn apply_prompt_cache_key(body: &mut Value, key: Option<&str>) {
    if let Some(key) = key {
        body["prompt_cache_key"] = json!(key);
    }
}

pub(crate) fn apply_responses_reasoning(
    body: &mut Value,
    thinking: ThinkingConfig,
    model: &Model,
    dialect: &EffortDialect,
) {
    if let Some(effort) = thinking.effort_str(dialect, model) {
        let mut reasoning = json!({ "effort": effort });
        if effort != dialect::OFF {
            reasoning["summary"] = json!("auto");
        }
        body["reasoning"] = reasoning;
    }
}

pub(crate) fn convert_input(messages: &[Message]) -> Value {
    let mut input = Vec::new();

    for msg in messages {
        match msg.role {
            Role::User => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            input.push(json!({
                                "type": "message",
                                "role": "user",
                                "content": [{"type": "input_text", "text": text}]
                            }));
                        }
                        ContentBlock::Image { source } => {
                            input.push(json!({
                                "type": "message",
                                "role": "user",
                                "content": [{"type": "input_image", "image_url": source.to_data_url()}]
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
                        ContentBlock::ToolUse { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::OpenAiReasoning { .. } => {}
                    }
                }
            }
            Role::Assistant => {
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            tool_calls.push((id, name, input));
                        }
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::OpenAiReasoning { .. } => {}
                    }
                }

                if !text_parts.is_empty() {
                    let joined = text_parts.join("");
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": joined}]
                    }));
                }

                for (id, name, args) in tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": args.to_string(),
                    }));
                }
            }
        }
    }

    Value::Array(input)
}

pub(super) fn coding_plan_input(messages: &[Message]) -> Value {
    let mut input = Vec::new();
    for message in messages {
        if !matches!(message.role, Role::Assistant) {
            input.extend(
                convert_input(std::slice::from_ref(message))
                    .as_array()
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            continue;
        }
        for block in &message.content {
            match block {
                ContentBlock::OpenAiReasoning { item } => input.push(item.clone()),
                ContentBlock::Text { text } => input.push(json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":text}]})),
                ContentBlock::ToolUse { id, name, input: arguments, .. } => input.push(json!({"type":"function_call", "call_id":id, "name":name, "arguments":arguments.to_string()})),
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } | ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
            }
        }
    }
    Value::Array(input)
}

pub(super) fn replayable_reasoning(item: &Value) -> bool {
    item["type"] == "reasoning"
        && item["encrypted_content"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        && item
            .get("status")
            .is_none_or(|status| status == "completed")
}

fn output_item_content(item: &Value) -> Option<Vec<ContentBlock>> {
    match item["type"].as_str()? {
        "reasoning" if replayable_reasoning(item) => {
            Some(vec![ContentBlock::OpenAiReasoning { item: item.clone() }])
        }
        "message" if item["role"] == "assistant" => item["content"]
            .as_array()?
            .iter()
            .map(|part| {
                if part["type"] != "output_text" {
                    return None;
                }
                Some(ContentBlock::Text {
                    text: part["text"].as_str()?.into(),
                })
            })
            .collect(),
        "function_call" => Some(vec![ContentBlock::tool_use(
            item["call_id"].as_str()?,
            item["name"].as_str()?,
            serde_json::from_str(item["arguments"].as_str()?).ok()?,
        )]),
        _ => None,
    }
}

pub(super) fn output_content(output: &[Value], require_replay: bool) -> Option<Vec<ContentBlock>> {
    let mut content = Vec::new();
    let mut first_text = true;
    for item in output {
        let Some(blocks) = output_item_content(item) else {
            if require_replay {
                return None;
            }
            continue;
        };
        for mut block in blocks {
            if let ContentBlock::Text { text } = &mut block {
                if first_text {
                    *text = text.trim_start().into();
                    first_text = false;
                }
                if text.is_empty() {
                    continue;
                }
            }
            content.push(block);
        }
    }
    Some(content)
}

pub(crate) fn convert_tools(anthropic_tools: &Value) -> Value {
    let Some(tools) = anthropic_tools.as_array() else {
        return json!([]);
    };

    Value::Array(
        tools
            .iter()
            .filter_map(|t| {
                Some(json!({
                    "type": "function",
                    "name": t.get("name")?,
                    "description": t.get("description")?,
                    "parameters": tool_parameters(t),
                    "strict": false,
                }))
            })
            .collect(),
    )
}

static SUMMARY_REJECTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn init_summary_rejected() -> &'static Mutex<HashSet<String>> {
    SUMMARY_REJECTED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn summary_rejected(base: &str) -> bool {
    init_summary_rejected().lock().unwrap().contains(base)
}

fn reject_summary(base: &str) {
    init_summary_rejected()
        .lock()
        .unwrap()
        .insert(base.to_owned());
}

fn has_summary(body: &Value) -> bool {
    body.get("reasoning")
        .and_then(|reasoning| reasoning.get("summary"))
        .is_some()
}

fn strip_summary(body: &mut Value) {
    if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.remove("summary");
    }
}

pub(crate) async fn do_stream(
    client: &HttpClient,
    model: &crate::model::Model,
    body: &Value,
    event_tx: &Sender<ProviderEvent>,
    auth: &ResolvedAuth,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    do_stream_with_routing(client, model, body, event_tx, auth, stream_timeout, None).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn do_stream_with_routing(
    client: &HttpClient,
    model: &crate::model::Model,
    body: &Value,
    event_tx: &Sender<ProviderEvent>,
    auth: &ResolvedAuth,
    stream_timeout: Duration,
    routing: Option<&RoutingState>,
) -> Result<StreamResponse, AgentError> {
    let base = auth.base_url.as_deref().ok_or_else(|| AgentError::Config {
        message: "Responses API requires a base_url in auth".into(),
    })?;
    let mut body = Cow::Borrowed(body);
    if summary_rejected(base) && has_summary(&body) {
        strip_summary(body.to_mut());
    }
    let mut body = body.into_owned();
    let result = post_responses(
        client,
        model,
        &body,
        event_tx,
        auth,
        base,
        stream_timeout,
        routing,
    )
    .await;
    match result {
        Err(err) if has_summary(&body) && err.is_unsupported_reasoning_summary() => {
            reject_summary(base);
            strip_summary(&mut body);
            post_responses(
                client,
                model,
                &body,
                event_tx,
                auth,
                base,
                stream_timeout,
                routing,
            )
            .await
        }
        result => result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn post_responses(
    client: &HttpClient,
    model: &crate::model::Model,
    body: &Value,
    event_tx: &Sender<ProviderEvent>,
    auth: &ResolvedAuth,
    base: &str,
    stream_timeout: Duration,
    routing: Option<&RoutingState>,
) -> Result<StreamResponse, AgentError> {
    let json_body = serde_json::to_vec(body)?;

    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("{}{RESPONSES_PATH}", base.trim_end_matches('/')))
        .header("content-type", "application/json")
        .header("user-agent", super::super::user_agent());
    if let Some(value) = routing.and_then(RoutingState::value) {
        builder = builder.header(TURN_STATE_HEADER, value);
    }
    let request = auth.configure_request(builder).body(json_body)?;

    debug!(
        model = %model.id,
        provider = "OpenAI Coding Plan",
        "sending Responses API request"
    );

    let response = client.send_async(request).await?;
    let status = response.status().as_u16();
    if let Some(routing) = routing {
        routing.retain(
            response
                .headers()
                .get(TURN_STATE_HEADER)
                .and_then(|value| value.to_str().ok()),
            "http_header",
        );
    }

    if status == 200 {
        parse_sse_inner(
            BufReader::new(response.into_body()),
            event_tx,
            stream_timeout,
            routing,
            routing.is_some() || is_coding_plan(base),
        )
        .await
    } else {
        Err(AgentError::from_response(response).await)
    }
}

struct ToolAccumulator {
    output_index: u64,
    call_id: String,
    name: String,
    arguments: String,
}

#[cfg(test)]
pub(crate) async fn parse_sse(
    reader: impl AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    parse_sse_inner(reader, event_tx, stream_timeout, None, false).await
}

async fn parse_sse_inner(
    reader: impl AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
    routing: Option<&RoutingState>,
    retain_reasoning: bool,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();
    let mut accumulator = ResponseAccumulator {
        retain_reasoning,
        ..ResponseAccumulator::default()
    };
    let mut deadline = Instant::now() + stream_timeout;
    let mut current_event = String::new();
    while let Some(line) =
        crate::providers::next_sse_line(&mut lines, &mut deadline, stream_timeout).await?
    {
        if line.is_empty() {
            current_event.clear();
            continue;
        }
        if let Some(event) = line.strip_prefix("event:") {
            current_event = event.trim().into();
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            break;
        }
        let parsed: Value = serde_json::from_str(data)?;
        let event = if current_event.is_empty() {
            parsed["type"].as_str().unwrap_or_default()
        } else {
            &current_event
        };
        if let Some(routing) = routing {
            routing.observe(event, &parsed);
        }
        if accumulator.push(event, &parsed, event_tx).await? {
            break;
        }
    }
    accumulator.finish()
}

pub(super) struct ResponseAccumulator {
    text: String,
    reasoning_text: String,
    tool_accumulators: Vec<ToolAccumulator>,
    usage: TokenUsage,
    stop_reason: Option<StopReason>,
    is_first_content: bool,
    pub(super) completed: Option<Value>,
    output_items: BTreeMap<u64, Value>,
    retain_reasoning: bool,
}

impl Default for ResponseAccumulator {
    fn default() -> Self {
        Self {
            text: String::new(),
            reasoning_text: String::new(),
            tool_accumulators: Vec::new(),
            usage: TokenUsage::default(),
            stop_reason: None,
            is_first_content: true,
            completed: None,
            output_items: BTreeMap::new(),
            retain_reasoning: false,
        }
    }
}

impl ResponseAccumulator {
    pub(super) fn coding_plan() -> Self {
        Self {
            retain_reasoning: true,
            ..Self::default()
        }
    }

    pub(super) async fn push(
        &mut self,
        event: &str,
        parsed: &Value,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<bool, AgentError> {
        if event == "error" {
            if let Ok(error) =
                serde_json::from_value::<crate::providers::SseErrorPayload>(parsed.clone())
            {
                return Err(error.into_agent_error());
            }
            return Err(AgentError::api(
                500,
                parsed["message"].as_str().unwrap_or("unknown error"),
            ));
        }
        match event {
            "response.output_text.delta" => {
                if let Some(delta) = parsed["delta"].as_str()
                    && !delta.is_empty()
                {
                    let delta = if self.is_first_content {
                        self.is_first_content = false;
                        delta.trim_start().to_string()
                    } else {
                        delta.to_string()
                    };
                    if !delta.is_empty() {
                        self.text.push_str(&delta);
                        event_tx
                            .send_async(ProviderEvent::TextDelta { text: delta })
                            .await?;
                    }
                }
            }

            "response.output_item.added" => {
                let item = &parsed["item"];
                let output_index = parsed["output_index"]
                    .as_u64()
                    .unwrap_or(self.tool_accumulators.len() as u64);
                if item["type"].as_str() == Some("function_call") {
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    if !name.is_empty() {
                        event_tx
                            .send_async(ProviderEvent::ToolUseStart {
                                id: call_id.clone(),
                                name: name.clone(),
                            })
                            .await?;
                    }
                    self.tool_accumulators.push(ToolAccumulator {
                        output_index,
                        call_id,
                        name,
                        arguments: String::new(),
                    });
                }
            }

            "response.function_call_arguments.delta" => {
                let delta: Cow<'_, str> = if let Some(s) = parsed["delta"].as_str() {
                    Cow::Borrowed(s)
                } else if let Some(obj) = parsed["delta"].as_object() {
                    Cow::Owned(serde_json::to_string(obj).unwrap_or_default())
                } else {
                    Cow::Borrowed("")
                };
                if !delta.is_empty() {
                    let acc = if let Some(idx) = parsed["output_index"].as_u64() {
                        self.tool_accumulators
                            .iter_mut()
                            .find(|a| a.output_index == idx)
                    } else {
                        self.tool_accumulators.last_mut()
                    };
                    if let Some(acc) = acc {
                        acc.arguments.push_str(&delta);
                    }
                }
            }

            "response.in_progress" => {
                if let Some(pp) = parsed.get("prompt_progress") {
                    let processed = pp["processed"].as_u64().unwrap_or(0) as u32;
                    let total = pp["total"].as_u64().unwrap_or(0) as u32;
                    let cache = pp["cache"].as_u64().unwrap_or(0) as u32;
                    event_tx
                        .send_async(ProviderEvent::PromptProgress {
                            processed,
                            total,
                            cache,
                        })
                        .await?;
                }
            }

            "response.output_item.done" => {
                let item = &parsed["item"];
                let index = parsed["output_index"]
                    .as_u64()
                    .unwrap_or(self.output_items.len() as u64);
                self.output_items.insert(index, item.clone());
                if item["type"].as_str() == Some("function_call") {
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let arguments = if let Some(s) = item["arguments"].as_str() {
                        s.to_string()
                    } else if let Some(obj) = item["arguments"].as_object() {
                        serde_json::to_string(obj).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let acc = if let Some(idx) = parsed["output_index"].as_u64() {
                        self.tool_accumulators
                            .iter_mut()
                            .find(|acc| acc.output_index == idx)
                    } else {
                        self.tool_accumulators.last_mut()
                    };
                    if let Some(acc) = acc {
                        let should_emit_start = acc.name.is_empty() && !name.is_empty();
                        if acc.call_id.is_empty() {
                            acc.call_id = call_id.clone();
                        }
                        if acc.name.is_empty() {
                            acc.name = name.clone();
                        }
                        if !arguments.is_empty() {
                            acc.arguments = arguments;
                        }
                        if should_emit_start {
                            event_tx
                                .send_async(ProviderEvent::ToolUseStart {
                                    id: acc.call_id.clone(),
                                    name: acc.name.clone(),
                                })
                                .await?;
                        }
                    } else {
                        if !name.is_empty() {
                            event_tx
                                .send_async(ProviderEvent::ToolUseStart {
                                    id: call_id.clone(),
                                    name: name.clone(),
                                })
                                .await?;
                        }
                        self.tool_accumulators.push(ToolAccumulator {
                            output_index: self.tool_accumulators.len() as u64,
                            call_id,
                            name,
                            arguments,
                        });
                    }
                }
            }

            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = parsed["delta"].as_str()
                    && !delta.is_empty()
                {
                    self.reasoning_text.push_str(delta);
                    event_tx
                        .send_async(ProviderEvent::ThinkingDelta {
                            text: delta.to_string(),
                        })
                        .await?;
                }
            }

            "response.reasoning_summary_part.added" if !self.reasoning_text.is_empty() => {
                self.reasoning_text.push_str("\n\n");
            }

            "response.completed" => {
                let resp = &parsed["response"];
                let mut completed = resp.clone();
                if completed["output"].as_array().is_none_or(Vec::is_empty) {
                    completed["output"] =
                        Value::Array(self.output_items.values().cloned().collect());
                }
                self.completed = Some(completed);

                if let Some(u) = resp.get("usage") {
                    self.usage = parse_usage(u);
                }

                let status = resp["status"].as_str().unwrap_or("completed");
                self.stop_reason = Some(match status {
                    "completed" => {
                        if self.tool_accumulators.is_empty() {
                            StopReason::EndTurn
                        } else {
                            StopReason::ToolUse
                        }
                    }
                    "incomplete" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                });
            }

            "response.incomplete" => {
                let resp = &parsed["response"];
                let mut completed = resp.clone();
                if completed["output"].as_array().is_none_or(Vec::is_empty) {
                    completed["output"] =
                        Value::Array(self.output_items.values().cloned().collect());
                }
                self.completed = Some(completed);
                if let Some(u) = resp.get("usage") {
                    self.usage = parse_usage(u);
                }
                self.stop_reason = Some(StopReason::MaxTokens);
            }

            "response.failed" => {
                let error = &parsed["response"]["error"];
                let message = error["message"]
                    .as_str()
                    .unwrap_or("response generation failed")
                    .to_string();
                let status = error["code"]
                    .as_str()
                    .and_then(sse_error_status)
                    .unwrap_or(FAILED_RESPONSE_STATUS);
                return Err(AgentError::api(status, message));
            }

            _ => {}
        }
        Ok(matches!(
            event,
            "response.completed" | "response.incomplete"
        ))
    }

    pub(super) fn replay_output(&self) -> Option<&Value> {
        let completed = self.completed.as_ref()?;
        let output = completed["output"].as_array()?;
        if output.is_empty() {
            return None;
        }
        let content = output_content(output, true)?;
        let text: String = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if text != self.text {
            return None;
        }
        let tools: Vec<_> = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => Some((id, name, input)),
                _ => None,
            })
            .collect();
        if tools.len() != self.tool_accumulators.len()
            || !self.tool_accumulators.iter().all(|acc| {
                tools.iter().any(|(id, name, input)| {
                    **id == acc.call_id
                        && **name == acc.name
                        && serde_json::from_str::<Value>(&acc.arguments).ok().as_ref()
                            == Some(*input)
                })
            })
        {
            return None;
        }
        Some(completed)
    }

    pub(super) fn finish(self) -> Result<StreamResponse, AgentError> {
        if self.stop_reason.is_none() {
            return Err(AgentError::api(
                500,
                "Responses stream ended before a terminal event",
            ));
        }
        let mut content_blocks: Vec<ContentBlock> = Vec::new();

        if !self.reasoning_text.is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                thinking: self.reasoning_text,
                signature: None,
            });
        }

        if !self.text.is_empty() {
            content_blocks.push(ContentBlock::Text { text: self.text });
        }

        for acc in self.tool_accumulators {
            let input: Value = match serde_json::from_str(&acc.arguments) {
                Ok(v) => {
                    debug!(tool = %acc.name, json = %acc.arguments, "tool input JSON");
                    v
                }
                Err(e) => {
                    warn!(error = %e, tool = %acc.name, json = %acc.arguments, "malformed tool JSON, falling back to {{}}");
                    Value::Object(Default::default())
                }
            };
            content_blocks.push(ContentBlock::tool_use(acc.call_id, acc.name, input));
        }

        if self.retain_reasoning
            && let Some(output) = self
                .completed
                .as_ref()
                .and_then(|response| response["output"].as_array())
            && !output.is_empty()
            && let Some(ordered) = output_content(output, false)
        {
            let mut retained: Vec<_> = content_blocks
                .iter()
                .filter(|block| matches!(block, ContentBlock::Thinking { .. }))
                .cloned()
                .collect();
            retained.extend(ordered);
            content_blocks = retained;
        }
        let stop_reason = if self.stop_reason == Some(StopReason::EndTurn)
            && content_blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        {
            Some(StopReason::ToolUse)
        } else {
            self.stop_reason
        };
        Ok(StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: content_blocks,
                ..Default::default()
            },
            usage: self.usage,
            stop_reason,
        })
    }
}

fn parse_usage(u: &Value) -> TokenUsage {
    let input_tokens = u["input_tokens"].as_u64().unwrap_or(0) as u32;
    let output_tokens = u["output_tokens"].as_u64().unwrap_or(0) as u32;

    let cached = u["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;
    let cache_write = u["input_tokens_details"]["cache_write_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;

    TokenUsage {
        input: input_tokens
            .saturating_sub(cached)
            .saturating_sub(cache_write),
        output: output_tokens,
        cache_read: cached,
        cache_creation: cache_write,
        cost: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::io::{AsyncReadExt, AsyncWriteExt, Cursor};
    use serde_json::json;
    use smol::net::TcpListener;
    use test_case::test_case;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);
    const OVERLOAD_MESSAGE: &str = "Our servers are currently overloaded. Please try again later.";
    const BAD_REQUEST_MESSAGE: &str = "Invalid value for 'model'";
    const RATE_LIMIT_MESSAGE: &str = "Rate limit hit";
    const TOOL_NAME: &str = "word_count";
    const TOOL_DESCRIPTION: &str = "Count words.";
    const TOOL_MUST_SURVIVE: &str = "a tool without a schema still belongs in the request";

    #[test]
    fn strip_summary_keeps_effort() {
        let mut body = json!({"reasoning": {"effort": "high", "summary": "auto"}});
        assert!(has_summary(&body));
        strip_summary(&mut body);
        assert!(!has_summary(&body));
        assert_eq!(body, json!({"reasoning": {"effort": "high"}}));
    }

    #[test]
    fn summary_memo_records_rejection_once() {
        let base = "https://summary-memo.test/v1";
        assert!(!summary_rejected(base));
        reject_summary(base);
        assert!(summary_rejected(base));
    }

    #[test]
    fn convert_tools_defaults_missing_parameters() {
        let tools = json!([{ "name": TOOL_NAME, "description": TOOL_DESCRIPTION }]);
        let converted = convert_tools(&tools);
        assert_eq!(
            converted[0]["name"],
            json!(TOOL_NAME),
            "{TOOL_MUST_SURVIVE}"
        );
        assert_eq!(
            converted[0]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn prompt_cache_key_is_optional() {
        let model = Model::from_spec("openai/gpt-5.6-luna").unwrap();
        let mut body = build_body(&model, &[], "", &json!([]));

        apply_prompt_cache_key(&mut body, Some("cache-probe"));
        assert_eq!(body["prompt_cache_key"], "cache-probe");

        let mut body = build_body(&model, &[], "", &json!([]));
        apply_prompt_cache_key(&mut body, None);
        assert!(body.get("prompt_cache_key").is_none());
    }

    async fn run_sse(sse: &str) -> (Result<StreamResponse, AgentError>, Vec<ProviderEvent>) {
        let (tx, rx) = flume::unbounded();
        let result = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT).await;
        (result, rx.drain().collect())
    }

    #[test]
    fn parse_sse_text_and_usage() {
        smol::block_on(async {
            let sse = "\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\" world\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":40,\"cache_write_tokens\":20}}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert_eq!(resp.usage.input, 40);
            assert_eq!(resp.usage.output, 10);
            assert_eq!(resp.usage.cache_read, 40);
            assert_eq!(resp.usage.cache_creation, 20);
            assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
            );

            let deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(deltas, vec!["Hello", " world"]);
        })
    }

    #[test]
    fn parse_sse_tool_calls() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\"}}\n\
\n\
event: response.output_item.added\n\
data: {\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"c2\",\"name\":\"read\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"output_index\":0,\"delta\":\"{\\\"command\\\": \\\"ls\\\"}\"}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"output_index\":1,\"delta\":\"{\\\"path\\\": \\\"/tmp\\\"}\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 2);
            assert_eq!((tools[0].0, tools[0].1), ("c1", "bash"));
            assert_eq!(tools[0].2["command"], "ls");
            assert_eq!((tools[1].0, tools[1].1), ("c2", "read"));
            assert_eq!(tools[1].2["path"], "/tmp");
            assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));

            let starts: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::ToolUseStart { id, name } => Some((id.as_str(), name.as_str())),
                    _ => None,
                })
                .collect();
            assert_eq!(starts, vec![("c1", "bash"), ("c2", "read")]);
        })
    }

    // Codex hides an overload in `code` on an otherwise healthy 200 stream, so these tags are what
    // decide between backing off and giving up: https://github.com/tontinton/maki/issues/777
    #[test_case("service_unavailable_error", "server_is_overloaded", OVERLOAD_MESSAGE, 529, true ; "codex_overload")]
    #[test_case("overloaded_error", "", OVERLOAD_MESSAGE, 529, true                              ; "overload_without_code")]
    #[test_case("invalid_request_error", "invalid_value", BAD_REQUEST_MESSAGE, 400, false        ; "bad_request")]
    fn parse_sse_error_event(
        error_type: &str,
        code: &str,
        message: &str,
        status: u16,
        retryable: bool,
    ) {
        smol::block_on(async {
            let data = json!({
                "type": "error",
                "error": { "type": error_type, "code": code, "message": message },
            });

            let (result, _) = run_sse(&format!("event: error\ndata: {data}\n\n")).await;
            let err = result.unwrap_err();
            assert_eq!(err.to_string(), format!("API error ({status}): {message}"));
            assert_eq!(err.is_retryable(), retryable);
        })
    }

    #[test_case("rate_limit_exceeded", RATE_LIMIT_MESSAGE, 429 ; "rate_limit")]
    #[test_case("server_is_overloaded", OVERLOAD_MESSAGE, 529  ; "overload")]
    fn parse_sse_response_failed(code: &str, message: &str, status: u16) {
        smol::block_on(async {
            let data = json!({
                "response": { "error": { "code": code, "message": message } },
            });

            let (result, _) = run_sse(&format!("event: response.failed\ndata: {data}\n\n")).await;
            let err = result.unwrap_err();
            assert_eq!(err.to_string(), format!("API error ({status}): {message}"));
            assert!(err.is_retryable());
        })
    }

    #[test]
    fn parse_sse_incomplete_response() {
        smol::block_on(async {
            let sse = "\
event: response.output_text.delta\n\
data: {\"delta\":\"partial\"}\n\
\n\
event: response.incomplete\n\
data: {\"response\":{\"status\":\"incomplete\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();
            assert_eq!(resp.stop_reason, Some(StopReason::MaxTokens));
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "partial")
            );
        })
    }

    #[test]
    fn convert_input_structure() {
        let messages = vec![
            Message::user("hello".to_string()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "thinking...".to_string(),
                    },
                    ContentBlock::tool_use("tc_1", "bash", json!({"command": "ls"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tc_1".to_string(),
                    content: "file.txt".to_string(),
                    is_error: false,
                }],
                ..Default::default()
            },
        ];

        let input = convert_input(&messages);
        let items = input.as_array().unwrap();

        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "hello");

        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[1]["content"][0]["type"], "output_text");
        assert_eq!(items[1]["content"][0]["text"], "thinking...");

        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "tc_1");
        assert_eq!(items[2]["name"], "bash");

        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["call_id"], "tc_1");
        assert_eq!(items[3]["output"], "file.txt");
    }

    #[test]
    fn parse_sse_reasoning_text_delta() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[],\"content\":[],\"encrypted_content\":\"\",\"status\":\"in_progress\"}}\n\
\n\
event: response.reasoning_text.delta\n\
data: {\"delta\":\"Let me think\"}\n\
\n\
event: response.reasoning_text.delta\n\
data: {\"delta\":\" about this\"}\n\
\n\
event: response.output_item.added\n\
data: {\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"content\":[],\"role\":\"assistant\"}}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello world\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"input_tokens_details\":{\"cached_tokens\":10},\"output_tokens_details\":{\"reasoning_tokens\":5}}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert_eq!(resp.usage.input, 90);
            assert_eq!(resp.usage.output, 20);
            assert_eq!(resp.usage.cache_read, 10);

            assert_eq!(resp.message.content.len(), 2);
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think about this")
            );
            assert!(
                matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello world")
            );

            let thinking_deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::ThinkingDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking_deltas, vec!["Let me think", " about this"]);

            let text_deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(text_deltas, vec!["Hello world"]);
        })
    }

    #[test]
    fn parse_sse_reasoning_summary_text_delta() {
        smol::block_on(async {
            let sse = "\
event: response.reasoning_summary_text.delta\n\
data: {\"delta\":\"Summary part\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Answer\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
\n";

            let (resp, events) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Summary part")
            );

            let thinking_deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::ThinkingDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking_deltas, vec!["Summary part"]);
        })
    }

    #[test]
    fn parse_sse_reasoning_only_no_text() {
        smol::block_on(async {
            let sse = "\
event: response.reasoning_text.delta\n\
data: {\"delta\":\"Thinking only\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"output_tokens_details\":{\"reasoning_tokens\":5}}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert_eq!(resp.message.content.len(), 1);
            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Thinking only")
            );
            assert_eq!(resp.usage.output, 5);
        })
    }

    #[test]
    fn parse_sse_malformed_tool_json_yields_empty_object() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{broken\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();
            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "bash");
            assert_eq!(*tools[0].2, Value::Object(Default::default()));
        })
    }

    // llama.cpp's /v1/responses endpoint omits output_index in SSE events
    // (see https://github.com/ggml-org/llama.cpp/issues/20607)

    #[test]
    fn parse_sse_tool_call_without_output_index() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{\\\"command\\\": \\\"ls\\\"}\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].0, "c1");
            assert_eq!(tools[0].1, "bash");
            assert_eq!(tools[0].2["command"], "ls");
            assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
        })
    }

    #[test]
    fn parse_sse_sequential_tool_calls_without_output_index() {
        smol::block_on(async {
            // Simulates llama.cpp streaming two sequential tool calls without output_index
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{\\\"command\\\": \\\"ls\\\"}\"}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\": \\\"ls\\\"}\"}}\n\
\n\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c2\",\"name\":\"read\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{\\\"path\\\": \\\"/tmp\\\"}\"}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c2\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\": \\\"/tmp\\\"}\"}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 2);
            assert_eq!((tools[0].0, tools[0].1), ("c1", "bash"));
            assert_eq!(tools[0].2["command"], "ls");
            assert_eq!((tools[1].0, tools[1].1), ("c2", "read"));
            assert_eq!(tools[1].2["path"], "/tmp");
        })
    }

    #[test]
    fn parse_sse_tool_done_without_output_index_updates_last_acc() {
        smol::block_on(async {
            // done event without output_index should update the last accumulator
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"glob\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":\"{\\\"pattern\\\": \\\"*.rs\\\"}\"}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"glob\",\"arguments\":\"{\\\"pattern\\\": \\\"*.rs\\\", \\\"path\\\": \\\"src\\\"}\"}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "glob");
            assert_eq!(tools[0].2["pattern"], "*.rs");
            assert_eq!(tools[0].2["path"], "src");
        })
    }

    #[test]
    fn parse_sse_prompt_progress_events() {
        smol::block_on(async {
            let sse = "\
event: response.in_progress\n\
data: {\"prompt_progress\":{\"processed\":100,\"total\":1000,\"cache\":50}}\n\
\n\
event: response.in_progress\n\
data: {\"prompt_progress\":{\"processed\":500,\"total\":1000,\"cache\":50}}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10}}}\n\
\n";

            let (_resp, events) = run_sse(sse).await;

            let progress: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    ProviderEvent::PromptProgress {
                        processed,
                        total,
                        cache,
                    } => Some((*processed, *total, *cache)),
                    _ => None,
                })
                .collect();
            assert_eq!(progress, vec![(100, 1000, 50), (500, 1000, 50)]);
        })
    }

    #[test]
    fn parse_sse_done_arguments_as_json_object() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":{\"path\":\"/tmp/file.txt\"}}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}
\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "read");
            assert_eq!(tools[0].2["path"], "/tmp/file.txt");
        })
    }

    #[test]
    fn parse_sse_reasoning_summary_part_added() {
        smol::block_on(async {
            let sse = "\
event: response.reasoning_summary_part.added\n\
data: {\"id\":\"sp_1\"}\n\
\n\
event: response.reasoning_summary_text.delta\n\
data: {\"delta\":\"First part\"}\n\
\n\
event: response.reasoning_summary_part.added\n\
data: {\"id\":\"sp_2\"}\n\
\n\
event: response.reasoning_summary_text.delta\n\
data: {\"delta\":\"Second part\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\"Answer\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert!(
                matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "First part\n\nSecond part")
            );
        })
    }

    #[test]
    fn parse_sse_delta_arguments_as_json_object() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"grep\"}}\n\
\n\
event: response.function_call_arguments.delta\n\
data: {\"delta\":{\"pattern\":\"TODO\",\"path\":\"src\"}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}
\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "grep");
            assert_eq!(tools[0].2["pattern"], "TODO");
            assert_eq!(tools[0].2["path"], "src");
        })
    }

    #[test]
    fn parse_sse_done_object_args_overrides_empty_delta() {
        smol::block_on(async {
            let sse = "\
event: response.output_item.added\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"edit\"}}\n\
\n\
event: response.output_item.done\n\
data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"edit\",\"arguments\":{\"path\":\"foo.rs\",\"old_string\":\"a\",\"new_string\":\"b\"}}}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}
\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            let tools: Vec<_> = resp.message.tool_uses().collect();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].1, "edit");
            assert_eq!(tools[0].2["path"], "foo.rs");
            assert_eq!(tools[0].2["old_string"], "a");
            assert_eq!(tools[0].2["new_string"], "b");
        })
    }

    #[test]
    fn parse_sse_no_reasoning_tokens_in_usage() {
        smol::block_on(async {
            let sse = "\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":40}}}}\n\
\n";

            let (resp, _) = run_sse(sse).await;
            let resp = resp.unwrap();

            assert_eq!(resp.usage.input, 60);
            assert_eq!(resp.usage.output, 10);
            assert_eq!(resp.usage.cache_read, 40);
        })
    }
    const OPAQUE: &str = "encrypted-fixture";
    const REASONING_ID: &str = "rs_fixture";
    const OUTPUT_TEXT: &str = "answer";
    const CALL_ID: &str = "call_fixture";

    fn opaque_item() -> Value {
        json!({"type":"reasoning", "id":REASONING_ID, "summary":[], "encrypted_content":OPAQUE})
    }

    #[test_case(false ; "terminal")]
    #[test_case(true ; "item_done_fallback")]
    fn interleaved_reasoning_retains_output_order(fallback: bool) {
        smol::block_on(async {
            let (tx, _rx) = flume::unbounded();
            let mut accumulator = ResponseAccumulator::coding_plan();
            let output = vec![
                opaque_item(),
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":OUTPUT_TEXT}]}),
                json!({"type":"function_call","call_id":CALL_ID,"name":TOOL_NAME,"arguments":"{}"}),
                opaque_item(),
            ];
            for index in [3, 2, 1, 0, 0] {
                accumulator
                    .push(
                        "response.output_item.done",
                        &json!({"output_index":index,"item":output[index]}),
                        &tx,
                    )
                    .await
                    .unwrap();
            }
            accumulator.push("response.completed", &json!({"response":{"status":"completed","output":if fallback {vec![]} else {output.clone()}}}), &tx).await.unwrap();
            let message = accumulator.finish().unwrap().message;
            assert_eq!(
                coding_plan_input(std::slice::from_ref(&message)),
                json!(output)
            );
            assert_eq!(
                convert_input(std::slice::from_ref(&message))
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(message.first_text_content(), Some(OUTPUT_TEXT));
            let without_opaque = Message {
                content: message
                    .content
                    .iter()
                    .filter(|block| !block.is_thinking())
                    .cloned()
                    .collect(),
                ..message.clone()
            };
            assert_eq!(
                crate::tokens::estimate_message_tokens(&[message]),
                crate::tokens::estimate_message_tokens(&[without_opaque])
            );
        });
    }

    #[test_case(json!(null), None, false ; "missing_encryption")]
    #[test_case(json!(""), None, false ; "empty_encryption")]
    #[test_case(json!(OPAQUE), Some("in_progress"), false ; "unfinished")]
    #[test_case(json!(OPAQUE), Some("completed"), true ; "completed")]
    #[test_case(json!(OPAQUE), None, true ; "no_status")]
    fn only_complete_reasoning_replays(encryption: Value, status: Option<&str>, replay: bool) {
        let mut item = opaque_item();
        item["encrypted_content"] = encryption;
        if let Some(status) = status {
            item["status"] = json!(status);
        }
        assert_eq!(replayable_reasoning(&item), replay);
        assert_eq!(
            output_content(std::slice::from_ref(&item), true).is_some(),
            replay
        );
        assert_eq!(
            output_content(&[item], false).unwrap().len(),
            usize::from(replay)
        );
    }

    #[test_case(())]
    fn mismatched_stream_disables_continuation(_: ()) {
        smol::block_on(async {
            let (tx, _rx) = flume::unbounded();
            let mut accumulator = ResponseAccumulator::coding_plan();
            accumulator
                .push(
                    "response.output_text.delta",
                    &json!({"delta":OUTPUT_TEXT}),
                    &tx,
                )
                .await
                .unwrap();
            accumulator
                .push(
                    "response.completed",
                    &json!({"response":{"status":"completed","output":[opaque_item()]}}),
                    &tx,
                )
                .await
                .unwrap();
            assert!(accumulator.replay_output().is_none());
        });
    }

    #[test_case("response.metadata", false ; "response_event")]
    #[test_case("codex.response.metadata", false ; "codex_event")]
    #[test_case("response.metadata", true ; "header_before_event")]
    fn http_turn_state_feedback(event: &'static str, header: bool) {
        smol::block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let auth = ResolvedAuth::for_test(
                Some(format!("http://{}", listener.local_addr().unwrap())),
                vec![(ROUTING_HINT_HEADER.into(), "model=gpt-test".into())],
            );
            let server = smol::spawn(async move {
                for request_index in 0..3 {
                    let (mut tcp, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") {
                        tcp.read_exact(&mut byte).await.unwrap();
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    assert!(request.contains("x-codex-routing-hint: model=gpt-test"));
                    assert_eq!(
                        request.contains(&format!("{TURN_STATE_HEADER}: {OPAQUE}")),
                        request_index == 1
                    );
                    let length: usize = request
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    tcp.read_exact(&mut vec![0; length]).await.unwrap();
                    let body = format!(
                        "data: {}\n\ndata: {}\n\n",
                        json!({"type":event,"headers":{TURN_STATE_HEADER:if header { OUTPUT_TEXT } else { OPAQUE }}}),
                        json!({"type":"response.completed","response":{"status":"completed","output":[opaque_item()]}})
                    );
                    let turn_header = if header {
                        format!("{TURN_STATE_HEADER}: {OPAQUE}\r\n")
                    } else {
                        String::new()
                    };
                    tcp.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n{turn_header}\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            });
            let routing = RoutingState::default();
            let model = Model::from_spec("openai/gpt-5.6-luna").unwrap();
            let client = HttpClient::new().unwrap();
            let (tx, _rx) = flume::unbounded();
            for index in 0..3 {
                if index == 2 {
                    routing.clear();
                }
                let response = do_stream_with_routing(
                    &client,
                    &model,
                    &json!({"model":model.id,"input":[]}),
                    &tx,
                    &auth,
                    TEST_STREAM_TIMEOUT,
                    Some(&routing),
                )
                .await
                .unwrap();
                assert!(
                    matches!(&response.message.content[0], ContentBlock::OpenAiReasoning { item } if item == &opaque_item())
                );
            }
            assert_eq!(routing.value().as_deref(), Some(OPAQUE));
            server.await;
        });
    }
    #[test_case(json!({"type":"unknown"}) ; "unknown_item")]
    #[test_case(json!({"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"refused"}]}) ; "unsupported_message")]
    #[test_case(json!({"type":"function_call","arguments":"bad json"}) ; "malformed_tool")]
    fn unsupported_output_keeps_complete_reasoning(unsupported: Value) {
        let output = vec![opaque_item(), unsupported, opaque_item()];
        assert!(output_content(&output, true).is_none());
        let content = output_content(&output, false).unwrap();
        assert_eq!(content.len(), 2);
        assert!(content.iter().all(|block| matches!(block, ContentBlock::OpenAiReasoning { item } if item == &opaque_item())));
    }
}
