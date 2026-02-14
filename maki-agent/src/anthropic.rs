use std::io::{BufRead, BufReader};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};
use ureq::Agent;

use crate::auth;
use crate::model::Model;
use crate::provider::Provider;
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

const API_VERSION: &str = "2023-06-01";
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);
const MODELS_URL: &str = "https://api.anthropic.com/v1/models?limit=1000";

pub struct Anthropic {
    agent: Agent,
    auth: auth::ResolvedAuth,
}

impl Anthropic {
    pub fn new() -> Result<Self, AgentError> {
        let resolved = auth::resolve()?;
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self {
            agent,
            auth: resolved,
        })
    }

    fn apply_auth<B>(&self, req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        let mut req = req.header("anthropic-version", API_VERSION);
        for (key, value) in &self.auth.headers {
            req = req.header(key, value);
        }
        req
    }
}

impl Provider for Anthropic {
    fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<AgentEvent>,
    ) -> Result<StreamResponse, AgentError> {
        let body = json!({
            "model": model.id,
            "max_tokens": model.max_output_tokens,
            "system": system,
            "messages": messages,
            "tools": tools,
            "stream": true,
        });

        for attempt in 1..=MAX_RETRIES {
            debug!(attempt, "sending API request");

            let req = self.apply_auth(
                self.agent
                    .post(&self.auth.api_url)
                    .header("content-type", "application/json"),
            );
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
                return Err(AgentError::from_response(response));
            }

            return parse_sse(BufReader::new(response.into_body().into_reader()), event_tx);
        }

        unreachable!()
    }

    fn list_models(&self) -> Result<Vec<String>, AgentError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            let mut url = MODELS_URL.to_string();
            if let Some(cursor) = &after_id {
                url.push_str(&format!("&after_id={cursor}"));
            }

            let response = self.apply_auth(self.agent.get(&url)).call()?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response));
            }

            let page: ModelsPage = serde_json::from_reader(response.into_body().into_reader())?;
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

fn parse_sse(
    reader: impl BufRead,
    event_tx: &Sender<AgentEvent>,
) -> Result<StreamResponse, AgentError> {
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<PendingToolCall> = Vec::new();
    let mut current_tool_json = String::new();
    let mut current_event = String::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<String> = None;

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
                                event_tx.send(AgentEvent::TextDelta { text: text.clone() })?;
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
                            event_tx.send(AgentEvent::Error {
                                message: format!("failed to parse tool {name}: {e}"),
                            })?;
                        }
                    }
                    current_tool_json.clear();
                }
            }
            "message_delta" => {
                if let Ok(ev) = serde_json::from_str::<MessageDeltaEvent>(data) {
                    if let Some(u) = ev.usage {
                        usage.output = u.output_tokens;
                    }
                    if let Some(d) = ev.delta {
                        stop_reason = d.stop_reason.or(stop_reason);
                    }
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
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn parse_sse_text_and_usage() {
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
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\
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
        assert!(
            matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
        );
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));

        let deltas: Vec<String> = rx
            .try_iter()
            .filter_map(|e| {
                if let AgentEvent::TextDelta { text: t } = e {
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
        let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"bash\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"echo hi\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n";

        let (tx, _rx) = mpsc::channel();
        let resp = parse_sse(sse_data.as_bytes(), &tx).unwrap();

        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "tu_1");
        assert_eq!(resp.tool_calls[0].call.name(), "bash");
    }
}
