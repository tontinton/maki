use std::io::{BufRead, BufReader};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};
use ureq::Agent;

use crate::auth;
use crate::tool::ToolCall;
use crate::{
    AgentError, AgentEvent, ContentBlock, Message, PendingToolCall, Role, StreamResponse,
    TokenUsage,
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
    ToolUse { id: String, name: String },
}

#[derive(Deserialize)]
struct ContentBlockStartEvent {
    content_block: SseContentBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
}

#[derive(Deserialize)]
struct ContentBlockDeltaEvent {
    delta: Delta,
}

#[derive(Deserialize)]
struct MessageDeltaEvent {
    #[serde(default)]
    usage: Option<Usage>,
}

const API_VERSION: &str = "2023-06-01";
const MODEL: &str = "claude-sonnet-4-20250514";
const MAX_TOKENS: u32 = 8096;
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);

pub fn stream_message(
    messages: &[Message],
    system: &str,
    tools: &Value,
    event_tx: &Sender<AgentEvent>,
) -> Result<StreamResponse, AgentError> {
    let resolved = auth::resolve()?;

    let body = json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": messages,
        "tools": tools,
        "stream": true,
    });

    let agent: Agent = Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();

    for attempt in 1..=MAX_RETRIES {
        debug!(attempt, "sending API request");

        let mut req = agent
            .post(&resolved.api_url)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json");
        for (key, value) in &resolved.headers {
            req = req.header(key, value);
        }
        let response = req.send(body.to_string().as_str())?;

        let status = response.status().as_u16();

        if status == 429 || status >= 500 {
            warn!(status, attempt, "retryable API error");
            if attempt < MAX_RETRIES {
                thread::sleep(RETRY_DELAY);
                continue;
            }
            return Err(AgentError::Api {
                status,
                message: "max retries exceeded".to_string(),
            });
        }

        if status != 200 {
            let body_text = response
                .into_body()
                .read_to_string()
                .unwrap_or_else(|_| "unable to read error body".to_string());
            return Err(AgentError::Api {
                status,
                message: body_text,
            });
        }

        return parse_sse_stream(response.into_body(), event_tx);
    }

    unreachable!()
}

fn parse_sse_stream(
    body: ureq::Body,
    event_tx: &Sender<AgentEvent>,
) -> Result<StreamResponse, AgentError> {
    parse_sse(BufReader::new(body.into_reader()), event_tx)
}

fn parse_sse(
    reader: impl BufRead,
    event_tx: &Sender<AgentEvent>,
) -> Result<StreamResponse, AgentError> {
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<PendingToolCall> = Vec::new();
    let mut current_tool_json = String::new();
    let mut current_event = String::new();
    let mut usage = TokenUsage::default();

    for line in reader.lines() {
        let line = line?;

        if let Some(event_type) = line.strip_prefix("event: ") {
            current_event = event_type.to_string();
            continue;
        }

        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };

        match current_event.as_str() {
            "message_start" => {
                if let Ok(ev) = serde_json::from_str::<MessageStartEvent>(data)
                    && let Some(u) = ev.message.usage
                {
                    usage = TokenUsage::from(u);
                }
            }
            "content_block_start" => {
                if let Ok(ev) = serde_json::from_str::<ContentBlockStartEvent>(data) {
                    match ev.content_block {
                        SseContentBlock::Text => {
                            content_blocks.push(ContentBlock::Text {
                                text: String::new(),
                            });
                        }
                        SseContentBlock::ToolUse { id, name } => {
                            current_tool_json.clear();
                            content_blocks.push(ContentBlock::ToolUse {
                                id,
                                name,
                                input: Value::Null,
                            });
                        }
                    }
                }
            }
            "content_block_delta" => {
                if let Ok(ev) = serde_json::from_str::<ContentBlockDeltaEvent>(data) {
                    match ev.delta {
                        Delta::TextDelta { text } => {
                            if !text.is_empty() {
                                event_tx.send(AgentEvent::TextDelta(text.clone()))?;
                                if let Some(ContentBlock::Text { text: t }) =
                                    content_blocks.last_mut()
                                {
                                    t.push_str(&text);
                                }
                            }
                        }
                        Delta::InputJsonDelta { partial_json } => {
                            current_tool_json.push_str(&partial_json);
                        }
                    }
                }
            }
            "content_block_stop" => {
                if let Some(ContentBlock::ToolUse { id, name, input }) = content_blocks.last_mut() {
                    *input = serde_json::from_str(&current_tool_json).unwrap_or(Value::Null);

                    match ToolCall::from_api(name, input) {
                        Ok(tc) => tool_calls.push(PendingToolCall {
                            id: id.clone(),
                            call: tc,
                        }),
                        Err(e) => {
                            warn!(tool = %name, error = %e, "failed to parse tool call");
                            event_tx.send(AgentEvent::Error(format!(
                                "failed to parse tool {name}: {e}"
                            )))?;
                        }
                    }
                    current_tool_json.clear();
                }
            }
            "message_delta" => {
                if let Ok(ev) = serde_json::from_str::<MessageDeltaEvent>(data)
                    && let Some(u) = ev.usage
                {
                    usage.output = u.output_tokens;
                }
            }
            _ => {}
        }
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: content_blocks,
        },
        tool_calls,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn parse_sse_text_only() {
        let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":8}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":10}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

        let (tx, rx) = mpsc::channel();
        let resp = parse_sse(sse_data.as_slice(), &tx).unwrap();

        assert_eq!(
            resp.usage,
            TokenUsage {
                input: 42,
                output: 10,
                cache_creation: 5,
                cache_read: 8
            }
        );
        assert_eq!(resp.message.content.len(), 1);
        assert!(
            matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
        );
        assert!(resp.tool_calls.is_empty());

        let deltas: Vec<String> = rx
            .try_iter()
            .filter_map(|e| {
                if let AgentEvent::TextDelta(t) = e {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(deltas, vec!["Hello", " world"]);
    }

    #[test]
    fn parse_sse_tool_use() {
        let line1 = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":10}}}"#;
        let line2 = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"bash"}}"#;
        let line3 = r#"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#;
        let line4 = r#"data: {"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":" \"echo hi\"}"}}"#;
        let line5 = r#"data: {"type":"content_block_stop"}"#;
        let line6 = r#"data: {"type":"message_delta","usage":{"output_tokens":5}}"#;

        let sse_data = format!(
            "event: message_start\n{line1}\n\n\
             event: content_block_start\n{line2}\n\n\
             event: content_block_delta\n{line3}\n\n\
             event: content_block_delta\n{line4}\n\n\
             event: content_block_stop\n{line5}\n\n\
             event: message_delta\n{line6}\n"
        );

        let (tx, _rx) = mpsc::channel();
        let resp = parse_sse(sse_data.as_bytes(), &tx).unwrap();

        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "tu_1");
        assert_eq!(resp.tool_calls[0].call.name(), "bash");
        assert!(
            matches!(&resp.message.content[0], ContentBlock::ToolUse { id, name, .. } if id == "tu_1" && name == "bash")
        );
    }
}
