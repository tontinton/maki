use std::time::Instant;

use flume::Sender;
use futures_lite::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use isahc::{HttpClient, Request};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::providers::ResolvedAuth;
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse, TokenUsage,
};

const RESPONSES_PATH: &str = "/responses";

pub(crate) fn build_body(
    model: &crate::model::Model,
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
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }
            }
            Role::Assistant => {
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push((id, name, input));
                        }
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
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
                    "parameters": t.get("input_schema")?,
                    "strict": false,
                }))
            })
            .collect(),
    )
}

pub(crate) async fn do_stream(
    client: &HttpClient,
    model: &crate::model::Model,
    body: &Value,
    event_tx: &Sender<ProviderEvent>,
    auth: &ResolvedAuth,
) -> Result<StreamResponse, AgentError> {
    let base = auth.base_url.as_deref().ok_or_else(|| AgentError::Config {
        message: "Responses API requires a base_url in auth".into(),
    })?;
    let json_body = serde_json::to_vec(body)?;

    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("{base}{RESPONSES_PATH}"))
        .header("content-type", "application/json");
    for (key, value) in &auth.headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let request = builder.body(json_body)?;

    debug!(
        model = %model.id,
        provider = "OpenAI Coding Plan",
        "sending Responses API request"
    );

    let response = client.send_async(request).await?;
    let status = response.status().as_u16();

    if status == 200 {
        parse_sse(BufReader::new(response.into_body()), event_tx).await
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

pub(crate) async fn parse_sse(
    reader: impl AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();

    let mut text = String::new();
    let mut tool_accumulators: Vec<ToolAccumulator> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut is_first_content = true;
    let mut deadline = Instant::now() + crate::providers::SSE_TIMEOUT;
    let mut current_event = String::new();

    while let Some(line) = crate::providers::next_sse_line(&mut lines, &mut deadline).await? {
        if let Some(event_type) = line.strip_prefix("event: ") {
            current_event = event_type.trim().to_string();
            continue;
        }

        let data = match line.strip_prefix("data: ") {
            Some(d) => d.trim(),
            None => continue,
        };

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
            return Err(AgentError::Api {
                status: 500,
                message,
            });
        }

        match current_event.as_str() {
            "response.output_text.delta" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(delta) = parsed["delta"].as_str()
                    && !delta.is_empty()
                {
                    let delta = if is_first_content {
                        is_first_content = false;
                        delta.trim_start().to_string()
                    } else {
                        delta.to_string()
                    };
                    if !delta.is_empty() {
                        text.push_str(&delta);
                        event_tx
                            .send_async(ProviderEvent::TextDelta { text: delta })
                            .await?;
                    }
                }
            }

            "response.output_item.added" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let item = &parsed["item"];
                let output_index = parsed["output_index"].as_u64().unwrap_or(0);
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
                    tool_accumulators.push(ToolAccumulator {
                        output_index,
                        call_id,
                        name,
                        arguments: String::new(),
                    });
                }
            }

            "response.function_call_arguments.delta" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(delta) = parsed["delta"].as_str() {
                    let output_index = parsed["output_index"].as_u64().unwrap_or(0);
                    if let Some(acc) = tool_accumulators
                        .iter_mut()
                        .find(|a| a.output_index == output_index)
                    {
                        acc.arguments.push_str(delta);
                    }
                }
            }

            "response.completed" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let resp = &parsed["response"];

                if let Some(u) = resp.get("usage") {
                    usage = parse_usage(u);
                }

                let status = resp["status"].as_str().unwrap_or("completed");
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
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let resp = &parsed["response"];
                if let Some(u) = resp.get("usage") {
                    usage = parse_usage(u);
                }
                stop_reason = Some(StopReason::MaxTokens);
            }

            "response.failed" => {
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let resp = &parsed["response"];
                let error = &resp["error"];
                let message = error["message"]
                    .as_str()
                    .unwrap_or("response generation failed")
                    .to_string();
                let code = error["code"].as_str().unwrap_or("server_error");
                let status = match code {
                    "rate_limit_exceeded" => 429,
                    "server_error" => 500,
                    _ => 500,
                };
                return Err(AgentError::Api { status, message });
            }

            _ => {}
        }
    }

    let mut content_blocks: Vec<ContentBlock> = Vec::new();

    if !text.is_empty() {
        content_blocks.push(ContentBlock::Text { text });
    }

    for acc in tool_accumulators {
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
        content_blocks.push(ContentBlock::ToolUse {
            id: acc.call_id,
            name: acc.name,
            input,
        });
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: content_blocks,
            ..Default::default()
        },
        usage,
        stop_reason,
    })
}

fn parse_usage(u: &Value) -> TokenUsage {
    let input_tokens = u["input_tokens"].as_u64().unwrap_or(0) as u32;
    let output_tokens = u["output_tokens"].as_u64().unwrap_or(0) as u32;

    let cached = u["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;

    TokenUsage {
        input: input_tokens.saturating_sub(cached),
        output: output_tokens,
        cache_read: cached,
        cache_creation: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::io::Cursor;
    use serde_json::json;

    async fn run_sse(sse: &str) -> (Result<StreamResponse, AgentError>, Vec<ProviderEvent>) {
        let (tx, rx) = flume::unbounded();
        let result = parse_sse(Cursor::new(sse.as_bytes()), &tx).await;
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

    #[test]
    fn parse_sse_error_event() {
        smol::block_on(async {
            let sse = "\
event: error\n\
data: {\"error\":{\"message\":\"Server overloaded\",\"type\":\"overloaded_error\"}}\n\
\n";

            let (err, _) = run_sse(sse).await;
            match err.unwrap_err() {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 529);
                    assert_eq!(message, "Server overloaded");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
        })
    }

    #[test]
    fn parse_sse_response_failed() {
        smol::block_on(async {
            let sse = "\
event: response.failed\n\
data: {\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit hit\"}}}\n\
\n";

            let (err, _) = run_sse(sse).await;
            match err.unwrap_err() {
                AgentError::Api { status, message } => {
                    assert_eq!(status, 429);
                    assert_eq!(message, "Rate limit hit");
                }
                other => panic!("expected Api error, got: {other:?}"),
            }
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
                    ContentBlock::ToolUse {
                        id: "tc_1".to_string(),
                        name: "bash".to_string(),
                        input: json!({"command": "ls"}),
                    },
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
}
