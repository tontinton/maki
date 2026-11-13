use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use futures::TryStreamExt;
use reqwest::Client;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::model::Model;
use crate::providers::copilot::copilot_usd;
use crate::providers::openai_compat::{
    StreamedText, assistant_response, parse_tool_input, tool_parameters,
};
use crate::providers::{ResolvedAuth, SseBody, ok_sse, sse_error_status};
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
    replay_reasoning: bool,
) -> Value {
    let input = convert_input(messages, replay_reasoning);
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

pub(crate) fn convert_input(messages: &[Message], replay_reasoning: bool) -> Value {
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
                                "output": crate::types::non_empty_tool_result(content),
                            }));
                        }
                        ContentBlock::ToolUse { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::Unknown(_) => {}
                    }
                }
            }
            Role::Assistant => {
                let mut reasoning = String::new();
                let mut reasoning_id = None;
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Thinking {
                            thinking,
                            reasoning_id: id,
                            ..
                        } => {
                            reasoning.push_str(thinking);
                            reasoning_id = reasoning_id.or_else(|| id.clone());
                        }
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            tool_calls.push((id, name, input));
                        }
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::Unknown(_) => {}
                    }
                }

                if replay_reasoning && !reasoning.is_empty() {
                    let mut item = json!({
                        "type": "reasoning",
                        "summary": [],
                        "content": [{"type": "reasoning_text", "text": reasoning}]
                    });
                    if let Some(id) = reasoning_id {
                        item["id"] = json!(id);
                    }
                    input.push(item);
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

static SUMMARY_REJECTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn summary_rejected(base: &str) -> bool {
    SUMMARY_REJECTED.lock().unwrap().contains(base)
}

fn reject_summary(base: &str) {
    SUMMARY_REJECTED.lock().unwrap().insert(base.to_owned());
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

/// `headers` are the provider's own for this request: over maki's, under
/// the credentials.
pub(crate) async fn do_stream(
    client: &Client,
    model: &crate::model::Model,
    headers: &HeaderMap,
    body: &Value,
    event_tx: &UnboundedSender<ProviderEvent>,
    auth: &ResolvedAuth,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let base = auth.base_url.as_deref().ok_or_else(|| AgentError::Config {
        message: "Responses API requires a base_url in auth".into(),
    })?;
    let mut body = Cow::Borrowed(body);
    if summary_rejected(base) && has_summary(&body) {
        strip_summary(body.to_mut());
    }
    let result = post_responses(
        client,
        model,
        headers,
        &body,
        event_tx,
        auth,
        base,
        stream_timeout,
    )
    .await;
    match result {
        Err(err) if has_summary(&body) && err.is_unsupported_reasoning_summary() => {
            reject_summary(base);
            strip_summary(body.to_mut());
            post_responses(
                client,
                model,
                headers,
                &body,
                event_tx,
                auth,
                base,
                stream_timeout,
            )
            .await
        }
        result => result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn post_responses(
    client: &Client,
    model: &crate::model::Model,
    headers: &HeaderMap,
    body: &Value,
    event_tx: &UnboundedSender<ProviderEvent>,
    auth: &ResolvedAuth,
    base: &str,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let json_body = serde_json::to_vec(body)?;

    let request = auth.finish(
        client
            .post(format!("{base}{RESPONSES_PATH}"))
            .header("content-type", "application/json")
            .headers(headers.clone())
            .body(json_body),
    )?;

    debug!(
        model = %model.id,
        provider = "OpenAI Coding Plan",
        "sending Responses API request"
    );

    ok_sse(client, request, stream_timeout, |body| {
        parse_sse(body, event_tx)
    })
    .await
}

struct ToolAccumulator {
    output_index: u64,
    call_id: String,
    name: String,
    arguments: String,
}

pub(crate) async fn parse_sse(
    mut events: SseBody,
    event_tx: &UnboundedSender<ProviderEvent>,
) -> Result<StreamResponse, AgentError> {
    let mut streamed = StreamedText::default();
    let mut tool_accumulators: Vec<ToolAccumulator> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;

    while let Some(event) = events.try_next().await? {
        let Some(data) = event.data.as_deref().map(str::trim) else {
            continue;
        };
        let current_event = event.event.as_deref().map_or("", str::trim);

        if current_event == "error" {
            if let Ok(ev) = serde_json::from_str::<crate::providers::SseErrorPayload>(data) {
                warn!(error_type = %ev.error.r#type, message = %ev.error.message, "SSE error in stream");
                return Err(ev.into_agent_error());
            }
            let parsed: Value = serde_json::from_str(data).unwrap_or_default();
            let message = parsed["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string();
            return Err(AgentError::api(500, message));
        }

        let Ok(parsed) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let event = if current_event.is_empty() {
            parsed["type"].as_str().unwrap_or_default()
        } else {
            current_event
        };

        match event {
            "response.output_text.delta" => {
                if let Some(delta) = parsed["delta"].as_str() {
                    streamed.push_text(delta.to_owned(), event_tx)?;
                }
            }

            "response.output_item.added" => {
                let item = &parsed["item"];
                let output_index = parsed["output_index"]
                    .as_u64()
                    .unwrap_or(tool_accumulators.len() as u64);
                match item["type"].as_str() {
                    Some("function_call") => {
                        let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                        let name = item["name"].as_str().unwrap_or_default().to_string();
                        if !name.is_empty() {
                            event_tx.send(ProviderEvent::ToolUseStart {
                                id: call_id.clone(),
                                name: name.clone(),
                            })?;
                        }
                        tool_accumulators.push(ToolAccumulator {
                            output_index,
                            call_id,
                            name,
                            arguments: String::new(),
                        });
                    }
                    Some("reasoning") => {
                        streamed.reasoning_id = item["id"].as_str().map(str::to_owned);
                    }
                    _ => {}
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
                        tool_accumulators.iter_mut().find(|a| a.output_index == idx)
                    } else {
                        tool_accumulators.last_mut()
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
                    event_tx.send(ProviderEvent::PromptProgress {
                        processed,
                        total,
                        cache,
                    })?;
                }
            }

            "response.output_item.done" => {
                let item = &parsed["item"];
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
                        tool_accumulators
                            .iter_mut()
                            .find(|acc| acc.output_index == idx)
                    } else {
                        tool_accumulators.last_mut()
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
                            event_tx.send(ProviderEvent::ToolUseStart {
                                id: acc.call_id.clone(),
                                name: acc.name.clone(),
                            })?;
                        }
                    } else {
                        if !name.is_empty() {
                            event_tx.send(ProviderEvent::ToolUseStart {
                                id: call_id.clone(),
                                name: name.clone(),
                            })?;
                        }
                        tool_accumulators.push(ToolAccumulator {
                            output_index: tool_accumulators.len() as u64,
                            call_id,
                            name,
                            arguments,
                        });
                    }
                }
            }

            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = parsed["delta"].as_str() {
                    streamed.push_reasoning(delta.to_owned(), event_tx)?;
                }
            }

            "response.reasoning_summary_part.added" if !streamed.reasoning.is_empty() => {
                streamed.push_reasoning("\n\n".to_owned(), event_tx)?;
            }

            "response.completed" => {
                if let Some(parsed_usage) = parse_usage(&parsed) {
                    usage = parsed_usage;
                }
                let status = parsed["response"]["status"].as_str().unwrap_or("completed");
                stop_reason = Some(match status {
                    "completed" => {
                        if tool_accumulators.is_empty() {
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
                if let Some(parsed_usage) = parse_usage(&parsed) {
                    usage = parsed_usage;
                }
                stop_reason = Some(StopReason::MaxTokens);
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
    }

    let mut content_blocks = streamed.into_blocks();

    for acc in tool_accumulators {
        let input = parse_tool_input(&acc.name, &acc.arguments);
        content_blocks.push(ContentBlock::tool_use(acc.call_id, acc.name, input));
    }

    Ok(assistant_response(content_blocks, usage, stop_reason))
}

/// The usage a `response.completed` or `response.incomplete` event carries.
fn parse_usage(event: &Value) -> Option<TokenUsage> {
    let u = event["response"].get("usage")?;
    let input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = u["output_tokens"].as_u64().unwrap_or(0);

    let cached = u["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);

    Some(TokenUsage {
        input: input_tokens.saturating_sub(cached),
        output: output_tokens,
        cache_read: cached,
        cache_creation: 0,
        cost: copilot_usd(&event["copilot_usage"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::iter;
    use test_case::test_case;
    use tokio::sync::mpsc::unbounded_channel;

    use crate::test_support::{sse_body, sse_event};

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

    async fn run_sse(sse: &str) -> (Result<StreamResponse, AgentError>, Vec<ProviderEvent>) {
        let (tx, mut rx) = unbounded_channel();
        let result = parse_sse(sse_body(sse.as_bytes()), &tx).await;
        (result, iter::from_fn(|| rx.try_recv().ok()).collect())
    }

    const COPILOT_NANO_AIU: u64 = 375_000;
    const COPILOT_USD: f64 = 0.00000375;

    #[test_case("response.completed", "completed", StopReason::EndTurn, json!({"total_nano_aiu": COPILOT_NANO_AIU}), Some(COPILOT_USD) ; "completed")]
    #[test_case("response.incomplete", "incomplete", StopReason::MaxTokens, json!({"total_nano_aiu": COPILOT_NANO_AIU}), Some(COPILOT_USD) ; "incomplete")]
    #[test_case("response.completed", "completed", StopReason::EndTurn, json!({"token_details": []}), None ; "no_total")]
    #[test_case("response.completed", "completed", StopReason::EndTurn, json!({"total_nano_aiu": COPILOT_NANO_AIU as f64}), Some(COPILOT_USD) ; "float_total")]
    #[test_case("response.completed", "completed", StopReason::EndTurn, json!({"total_nano_aiu": "375000"}), None ; "string_total")]
    #[test_case("response.completed", "completed", StopReason::EndTurn, json!("billed"), None ; "not_an_object")]
    #[tokio::test]
    async fn parse_sse_copilot_usage_prices_the_turn(
        event: &str,
        status: &str,
        stop: StopReason,
        copilot_usage: Value,
        cost: Option<f64>,
    ) {
        let sse = sse_event(
            event,
            &json!({
                "copilot_usage": copilot_usage,
                "response": {"status": status, "usage": {"input_tokens": 100, "output_tokens": 10}},
            }),
        );

        let (resp, _events) = run_sse(&sse).await;
        let resp = resp.unwrap();

        assert_eq!(
            resp.usage,
            TokenUsage {
                input: 100,
                output: 10,
                cache_creation: 0,
                cache_read: 0,
                cost,
            }
        );
        assert_eq!(resp.stop_reason, Some(stop));
    }

    #[tokio::test]
    async fn parse_sse_text_and_usage() {
        let sse = "\
event: response.output_text.delta\n\
data: {\"delta\":\"Hello\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"delta\":\" world\"}\n\
\n\
event: response.completed\n\
data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":40}}}}\n\
\n";

        let (resp, events) = run_sse(sse).await;
        let resp = resp.unwrap();

        assert_eq!(resp.usage.input, 60);
        assert_eq!(resp.usage.output, 10);
        assert_eq!(resp.usage.cache_read, 40);
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
    }

    #[tokio::test]
    async fn parse_sse_tool_calls() {
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
    }

    // Codex hides an overload in `code` on an otherwise healthy 200 stream, so these tags are what
    // decide between backing off and giving up: https://github.com/tontinton/maki/issues/777
    #[test_case("service_unavailable_error", "server_is_overloaded", OVERLOAD_MESSAGE, 529, true ; "codex_overload")]
    #[test_case("overloaded_error", "", OVERLOAD_MESSAGE, 529, true                              ; "overload_without_code")]
    #[test_case("invalid_request_error", "invalid_value", BAD_REQUEST_MESSAGE, 400, false        ; "bad_request")]
    #[tokio::test]
    async fn parse_sse_error_event(
        error_type: &str,
        code: &str,
        message: &str,
        status: u16,
        retryable: bool,
    ) {
        let data = json!({
            "type": "error",
            "error": { "type": error_type, "code": code, "message": message },
        });

        let (result, _) = run_sse(&format!("event: error\ndata: {data}\n\n")).await;
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), format!("API error ({status}): {message}"));
        assert_eq!(err.is_retryable(), retryable);
    }

    #[test_case("rate_limit_exceeded", RATE_LIMIT_MESSAGE, 429 ; "rate_limit")]
    #[test_case("server_is_overloaded", OVERLOAD_MESSAGE, 529  ; "overload")]
    #[tokio::test]
    async fn parse_sse_response_failed(code: &str, message: &str, status: u16) {
        let data = json!({
            "response": { "error": { "code": code, "message": message } },
        });

        let (result, _) = run_sse(&format!("event: response.failed\ndata: {data}\n\n")).await;
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), format!("API error ({status}): {message}"));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn parse_sse_incomplete_response() {
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
                content: vec![ContentBlock::tool_result("tc_1", "file.txt", false)],
                ..Default::default()
            },
        ];

        let input = convert_input(&messages, false);
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
    fn convert_input_replays_past_reasoning() {
        let assistant = |reasoning_id: Option<String>| Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "weighing it".to_string(),
                    signature: None,
                    reasoning_id,
                },
                ContentBlock::Text {
                    text: "answer".to_string(),
                },
            ],
            ..Default::default()
        };
        let messages = [Message::user("hello".to_string()), assistant(None)];

        let input = convert_input(&messages, true);
        let items = input.as_array().unwrap();

        assert_eq!(items[1]["type"], "reasoning");
        assert_eq!(items[1]["summary"], json!([]));
        assert_eq!(items[1]["content"][0]["type"], "reasoning_text");
        assert_eq!(items[1]["content"][0]["text"], "weighing it");
        assert!(items[1].get("id").is_none());
        assert_eq!(items[2]["type"], "message");
        assert_eq!(items[2]["role"], "assistant");
        assert_eq!(items[2]["content"][0]["text"], "answer");

        let dropped = convert_input(&messages, false);
        assert_eq!(dropped[1]["type"], "message");
        assert_eq!(dropped[1]["role"], "assistant");

        let with_id = [
            Message::user("hello".to_string()),
            assistant(Some("rs_1".to_string())),
        ];
        let input = convert_input(&with_id, true);
        assert_eq!(input[1]["id"], "rs_1");
    }

    #[test]
    fn convert_input_blank_tool_result_gets_marker() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result("tc_1", "", false)],
            ..Default::default()
        }];

        let input = convert_input(&messages, false);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["output"], crate::types::EMPTY_RESPONSE_MARKER);
    }

    #[tokio::test]
    async fn parse_sse_reasoning_text_delta() {
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
            matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, reasoning_id, .. } if thinking == "Let me think about this" && reasoning_id.as_deref() == Some("rs_1"))
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
    }

    #[tokio::test]
    async fn parse_sse_reasoning_summary_text_delta() {
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
    }

    #[tokio::test]
    async fn parse_sse_reasoning_only_no_text() {
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
    }

    #[tokio::test]
    async fn parse_sse_malformed_tool_json_yields_empty_object() {
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
    }

    // llama.cpp's /v1/responses endpoint omits output_index in SSE events
    // (see https://github.com/ggml-org/llama.cpp/issues/20607)

    #[tokio::test]
    async fn parse_sse_tool_call_without_output_index() {
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
    }

    #[tokio::test]
    async fn parse_sse_sequential_tool_calls_without_output_index() {
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
    }

    #[tokio::test]
    async fn parse_sse_tool_done_without_output_index_updates_last_acc() {
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
    }

    #[tokio::test]
    async fn parse_sse_prompt_progress_events() {
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
    }

    #[tokio::test]
    async fn parse_sse_done_arguments_as_json_object() {
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
    }

    #[tokio::test]
    async fn parse_sse_reasoning_summary_part_added() {
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

        let (resp, events) = run_sse(sse).await;
        let resp = resp.unwrap();
        let thinking = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();

        const EXPECTED: &str = "First part\n\nSecond part";
        assert_eq!(thinking, EXPECTED);
        assert!(
            matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == EXPECTED)
        );
    }

    #[tokio::test]
    async fn parse_sse_delta_arguments_as_json_object() {
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
    }

    #[tokio::test]
    async fn parse_sse_done_object_args_overrides_empty_delta() {
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
    }

    #[tokio::test]
    async fn parse_sse_no_reasoning_tokens_in_usage() {
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
    }
}
