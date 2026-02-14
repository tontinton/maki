use std::env;
use std::io::{BufRead, BufReader};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};
use ureq::Agent;

use crate::model::Model;
use crate::provider::Provider;
use crate::tool::ToolCall;
use crate::{
    AgentError, AgentEvent, ContentBlock, Message, PendingToolCall, Role, StreamResponse,
    TokenUsage,
};

const API_KEY_ENV: &str = "Z_AI_API_KEY";
const COMPLETIONS_URL: &str = "https://api.z.ai/api/paas/v4/chat/completions";
const MODELS_URL: &str = "https://api.z.ai/api/paas/v4/models";
const MAX_RETRIES: u32 = 3;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);
const STREAM_DONE: &str = "[DONE]";

pub struct Zai {
    agent: Agent,
    api_key: String,
}

impl Zai {
    pub fn new() -> Result<Self, AgentError> {
        let api_key = env::var(API_KEY_ENV).map_err(|_| AgentError::Api {
            status: 0,
            message: format!("{API_KEY_ENV} not set"),
        })?;
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self { agent, api_key })
    }
}

fn retry_delay(attempt: u32) -> Duration {
    let base = INITIAL_RETRY_DELAY.as_millis() as u64 * 2u64.pow(attempt - 1);
    let capped = base.min(MAX_RETRY_DELAY.as_millis() as u64);
    let jitter = capped * 3 / 4;
    Duration::from_millis(jitter)
}

fn convert_messages(messages: &[Message], system: &str) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];

    for msg in messages {
        match msg.role {
            Role::User => {
                let mut tool_results = Vec::new();
                let mut text_parts = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }

                if !text_parts.is_empty() {
                    out.push(json!({"role": "user", "content": text_parts.join("\n")}));
                }
                out.extend(tool_results);
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": input.to_string(),
                                }
                            }));
                        }
                        ContentBlock::ToolResult { .. } => {}
                    }
                }

                let mut msg_obj = json!({"role": "assistant"});
                if !text.is_empty() {
                    msg_obj["content"] = Value::String(text);
                }
                if !tool_calls.is_empty() {
                    msg_obj["tool_calls"] = Value::Array(tool_calls);
                }
                out.push(msg_obj);
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

fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop" => "end_turn",
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        _ => "end_turn",
    }
}

impl Provider for Zai {
    fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<AgentEvent>,
    ) -> Result<StreamResponse, AgentError> {
        let wire_messages = convert_messages(messages, system);
        let wire_tools = convert_tools(tools);

        let mut body = json!({
            "model": model.id,
            "messages": wire_messages,
            "stream": true,
            "max_tokens": model.max_output_tokens,
        });
        if wire_tools.as_array().is_some_and(|a| !a.is_empty()) {
            body["tools"] = wire_tools;
        }
        let body_str = body.to_string();

        for attempt in 1..=MAX_RETRIES {
            debug!(attempt, "sending Z.AI API request");

            let req = self
                .agent
                .post(COMPLETIONS_URL)
                .header("content-type", "application/json")
                .header("authorization", &format!("Bearer {}", self.api_key));
            let response = req.send(body_str.as_str())?;
            let status = response.status().as_u16();

            if status == 429 || status >= 500 {
                let error_body = response.into_body().read_to_string().unwrap_or_default();
                if error_body.contains("1113") || error_body.contains("nsufficien") {
                    return Err(AgentError::Api {
                        status,
                        message: error_body,
                    });
                }
                warn!(status, attempt, body = %error_body, "retryable Z.AI API error");
                if attempt < MAX_RETRIES {
                    thread::sleep(retry_delay(attempt));
                    continue;
                }
                return Err(AgentError::Api {
                    status,
                    message: error_body,
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
        let response = self
            .agent
            .get(MODELS_URL)
            .header("authorization", &format!("Bearer {}", self.api_key))
            .call()?;
        if response.status().as_u16() != 200 {
            return Err(AgentError::from_response(response));
        }

        let body: Value = serde_json::from_reader(response.into_body().into_reader())?;
        let mut models: Vec<String> = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        models.sort();
        Ok(models)
    }
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Option<ChunkDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<ChunkUsage>,
}

struct ToolAccumulator {
    id: String,
    name: String,
    arguments: String,
}

fn parse_sse(
    reader: impl BufRead,
    event_tx: &Sender<AgentEvent>,
) -> Result<StreamResponse, AgentError> {
    let mut text = String::new();
    let mut tool_accumulators: Vec<ToolAccumulator> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<String> = None;

    for line in reader.lines() {
        let line = line?;
        let data = match line.strip_prefix("data: ") {
            Some(d) => d.trim(),
            None => continue,
        };

        if data == STREAM_DONE {
            break;
        }

        let chunk: SseChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        if let Some(u) = chunk.usage {
            usage = TokenUsage {
                input: u.prompt_tokens,
                output: u.completion_tokens,
                cache_read: u
                    .prompt_tokens_details
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0),
                cache_creation: 0,
            };
        }

        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };

        if let Some(reason) = choice.finish_reason {
            stop_reason = Some(map_finish_reason(&reason).to_string());
        }

        let Some(delta) = choice.delta else {
            continue;
        };

        if let Some(reasoning) = delta.reasoning_content
            && !reasoning.is_empty()
        {
            event_tx.send(AgentEvent::TextDelta {
                text: reasoning.clone(),
            })?;
            text.push_str(&reasoning);
        }

        if let Some(content) = delta.content
            && !content.is_empty()
        {
            event_tx.send(AgentEvent::TextDelta {
                text: content.clone(),
            })?;
            text.push_str(&content);
        }

        if let Some(tc_deltas) = delta.tool_calls {
            for tc in tc_deltas {
                while tool_accumulators.len() <= tc.index {
                    tool_accumulators.push(ToolAccumulator {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                }
                let acc = &mut tool_accumulators[tc.index];
                if let Some(id) = tc.id {
                    acc.id = id;
                }
                if let Some(func) = tc.function {
                    if let Some(name) = func.name {
                        acc.name = name;
                    }
                    if let Some(args) = func.arguments {
                        acc.arguments.push_str(&args);
                    }
                }
            }
        }
    }

    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<PendingToolCall> = Vec::new();

    if !text.is_empty() {
        content_blocks.push(ContentBlock::Text { text });
    }

    for acc in tool_accumulators {
        let input: Value = serde_json::from_str(&acc.arguments).unwrap_or(Value::Null);

        match ToolCall::from_api(&acc.name, &input) {
            Ok(tc) => tool_calls.push(PendingToolCall {
                id: acc.id.clone(),
                call: tc,
            }),
            Err(e) => {
                warn!(tool = %acc.name, error = %e, "failed to parse tool call");
                event_tx.send(AgentEvent::Error {
                    message: format!("failed to parse tool {}: {e}", acc.name),
                })?;
            }
        }

        content_blocks.push(ContentBlock::ToolUse {
            id: acc.id,
            name: acc.name,
            input,
        });
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
    use test_case::test_case;

    #[test]
    fn parse_sse_text_and_usage() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":40}}}\n\
\n\
data: [DONE]\n";

        let (tx, rx) = mpsc::channel();
        let resp = parse_sse(sse.as_bytes(), &tx).unwrap();

        assert_eq!(resp.usage.input, 100);
        assert_eq!(resp.usage.output, 10);
        assert_eq!(resp.usage.cache_read, 40);
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert!(
            matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
        );
        assert!(resp.tool_calls.is_empty());

        let deltas: Vec<String> = rx
            .try_iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Hello", " world"]);
    }

    #[test]
    fn parse_sse_reasoning_and_content() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me think\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"...\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\
\n\
data: [DONE]\n";

        let (tx, rx) = mpsc::channel();
        let resp = parse_sse(sse.as_bytes(), &tx).unwrap();

        assert!(
            matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Let me think...Hello")
        );

        let deltas: Vec<String> = rx
            .try_iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Let me think", "...", "Hello"]);
    }

    #[test]
    fn parse_sse_tool_use() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"command\\\":\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\" \\\"echo hi\\\"}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":15}}\n\
\n\
data: [DONE]\n";

        let (tx, _rx) = mpsc::channel();
        let resp = parse_sse(sse.as_bytes(), &tx).unwrap();

        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].call.name(), "bash");
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test_case("stop", "end_turn" ; "stop_maps_to_end_turn")]
    #[test_case("tool_calls", "tool_use" ; "tool_calls_maps_to_tool_use")]
    #[test_case("length", "max_tokens" ; "length_maps_to_max_tokens")]
    #[test_case("unknown", "end_turn" ; "unknown_defaults_to_end_turn")]
    fn finish_reason_mapping(input: &str, expected: &str) {
        assert_eq!(map_finish_reason(input), expected);
    }

    #[test]
    fn convert_messages_structure() {
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
            },
            Message::tool_results(vec![(
                "tc_1".to_string(),
                crate::ToolDoneEvent {
                    tool: "bash",
                    content: "file.txt".to_string(),
                    is_error: false,
                },
            )]),
        ];

        let wire = convert_messages(&messages, "be helpful");

        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[0]["content"], "be helpful");
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[1]["content"], "hello");
        assert_eq!(wire[2]["role"], "assistant");
        assert_eq!(wire[2]["content"], "thinking...");
        assert_eq!(wire[2]["tool_calls"][0]["id"], "tc_1");
        assert_eq!(wire[2]["tool_calls"][0]["type"], "function");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "tc_1");
        assert_eq!(wire[3]["content"], "file.txt");
    }

    #[test]
    fn retry_delay_exponential_backoff() {
        let d1 = retry_delay(1);
        let d2 = retry_delay(2);
        let d3 = retry_delay(3);
        assert!(d1 < d2);
        assert!(d2 < d3);
        assert!(d3 <= MAX_RETRY_DELAY);
    }

    #[test]
    fn convert_tools_structure() {
        let anthropic = json!([{
            "name": "bash",
            "description": "Run a command",
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }
        }]);

        let openai = convert_tools(&anthropic);
        let tool = &openai[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "bash");
        assert_eq!(tool["function"]["description"], "Run a command");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }
}
