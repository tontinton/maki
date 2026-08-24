//! The Command Code token plans (GOAT / Pro / Max / Team).
//!
//! Plans do not speak the OpenAI-compatible `/provider/v1/chat/completions`
//! endpoint that [`super::credits`] uses. They speak a custom SSE protocol at
//! `POST {base}/alpha/generate`: a request envelope of
//! `config`/`memory`/`taste`/`skills`/`params`/`threadId`, and a
//! Vercel-AI-SDK shaped event stream (`text-delta`, `reasoning-delta`,
//! `tool-call`, `finish`). Reverse-engineered from `pi-commandcode-provider`
//! against command-code CLI 1.15.1, not from token-plan API docs.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use maki_storage::id::{MakiId, SessionRef};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{Model, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, RequestOptions, Role, StopReason,
    StreamResponse, TokenUsage,
};

use super::super::{KeyPool, ResolvedAuth, Timeouts, http_client, next_sse_line, user_agent};
use super::{
    BASE_URL, ENV_VAR, MAX_GENERATE_TOKENS, PLAN_SLUG, reasoning_effort, resolve_auth_from_key,
    resolve_key_pool,
};

/// Sent as `x-command-code-version`; the endpoint gates behaviour on it.
const CLI_VERSION: &str = "1.15.1";
const DEFAULT_TEMPERATURE: f64 = 0.3;

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: PLAN_SLUG,
    display_name: "Command Code",
    protocol: maki_config::providers::Protocol::CommandCode,
    default_base_url: BASE_URL,
    default_api_key_env: ENV_VAR,
    default_model: "command-code/claude-opus-5",
    plans: None,
    login_url: Some("https://commandcode.ai"),
    needs_url: false,
});

pub struct CommandCode {
    client: HttpClient,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    stream_timeout: Duration,
}

impl CommandCode {
    pub fn new(timeouts: Timeouts) -> Result<Self, AgentError> {
        let pool = resolve_key_pool(PLAN_SLUG)?;
        let config = maki_config::providers::ProvidersConfig::load();
        let base_url = maki_config::providers::resolve_base_url(PLAN_SLUG, config.get(PLAN_SLUG));
        Ok(Self {
            client: http_client(timeouts),
            auth: Arc::new(Mutex::new(resolve_auth_from_key(pool.current(), base_url))),
            key_pool: Some(pool),
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: Timeouts) -> Self {
        Self {
            client: http_client(timeouts),
            auth,
            key_pool: None,
            stream_timeout: timeouts.stream,
        }
    }

    /// Live, so a base URL a caller sets on the shared auth after construction
    /// is honoured instead of a snapshot taken in the constructor.
    fn base_url(&self) -> Option<String> {
        self.auth.lock().unwrap().base_url.clone()
    }

    fn base(&self) -> String {
        self.auth
            .lock()
            .unwrap()
            .base_url
            .as_deref()
            .unwrap_or(BASE_URL)
            .trim_end_matches('/')
            .to_string()
    }

    fn build_request(&self, method: &str, url: &str) -> isahc::http::request::Builder {
        let auth = self.auth.lock().unwrap();
        auth.configure_request(
            Request::builder()
                .method(method)
                .uri(url)
                .header("user-agent", user_agent())
                .header("x-command-code-version", CLI_VERSION)
                .header("x-cli-environment", "production"),
        )
    }

    // Mirrors the trait's request shape; splitting it would only move the
    // same arguments one call deeper.
    #[allow(clippy::too_many_arguments)]
    fn build_body(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        opts: RequestOptions,
        working_dir: &str,
        session_id: Option<&SessionRef>,
    ) -> Value {
        let mut params = json!({
            "model": model.id,
            "messages": convert_messages(messages),
            "tools": convert_tools(tools),
            "system": system,
            "max_tokens": model
                .max_output_tokens
                .unwrap_or(MAX_GENERATE_TOKENS)
                .min(MAX_GENERATE_TOKENS),
            "temperature": DEFAULT_TEMPERATURE,
            "stream": true,
        });
        if let Some(effort) = reasoning_effort(model, opts.thinking) {
            params["reasoning_effort"] = json!(effort);
        }

        json!({
            // The endpoint expects the CLI's project envelope. maki builds its
            // own system prompt, so only the working directory and date carry
            // real information; the git fields stay empty rather than
            // duplicating what the prompt already says.
            "config": {
                "workingDir": working_dir,
                "date": jiff::Zoned::now().date().to_string(),
                "environment": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                "structure": [],
                "isGitRepo": false,
                "currentBranch": "",
                "mainBranch": "",
                "gitStatus": "",
                "recentCommits": [],
            },
            "memory": Value::Null,
            "taste": Value::Null,
            "skills": Value::Null,
            "params": params,
            "threadId": thread_id(session_id),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn do_stream(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        // One getcwd per request: the envelope and the project-slug header
        // must agree, and they cannot if each reads the cwd separately.
        let working_dir = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let body = serde_json::to_vec(&self.build_body(
            model,
            messages,
            system,
            tools,
            opts,
            &working_dir,
            session_id,
        ))?;
        let url = format!("{}/alpha/generate", self.base());
        let request = self
            .build_request("POST", &url)
            .header("content-type", "application/json")
            // maki has no taste/co features, so never opt this session into
            // training them.
            .header("x-project-slug", project_slug(&working_dir))
            .header("x-taste-learning", "false")
            .header("x-co-flag", "false")
            .body(body)?;

        let response = self.client.send_async(request).await?;
        if response.status().as_u16() == 200 {
            parse_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(api_error(response).await)
        }
    }
}

/// Command Code reports HTTP failures as
/// `{"success":false,"error":{"code","status","message"}}`. Lifting the message
/// out keeps a plan or quota refusal readable — "MODEL_NOT_IN_PLAN: ..." rather
/// than a wall of JSON the user has to parse by eye.
async fn api_error(mut response: isahc::Response<isahc::AsyncBody>) -> AgentError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| Some(v.pointer("/error/message")?.as_str()?.to_string()))
        .unwrap_or(body);
    AgentError::Api { status, message }
}

/// Command Code groups requests server-side by project. The reference client
/// derives the slug from the working directory path, so the same directory has
/// to produce the same slug here.
fn project_slug(path: &str) -> String {
    // Exactly one leading letter plus a colon, so a Windows drive is stripped
    // but a path segment like `mydir:file` keeps its head.
    let path = path
        .strip_prefix(|c: char| c.is_ascii_alphabetic())
        .and_then(|rest| rest.strip_prefix(':'))
        .unwrap_or(path);
    // `slugify` folds on Unicode alphanumerics where the reference client folds
    // on ASCII, so a non-ASCII path groups under a different key server-side
    // than the CLI would pick. A grouping label, not a request failure.
    let slug = maki_config::providers::slugify(path);
    if slug.is_empty() {
        "project".into()
    } else {
        slug
    }
}

/// The conversation key the endpoint caches against, so it has to be stable
/// across a session's turns: usage comes back with `cacheReadTokens`, and a
/// fresh id every request would make each turn look like a new thread.
///
/// Only a provider call outside any session falls back to a fresh id.
fn thread_id(session_id: Option<&SessionRef>) -> String {
    session_id.map_or_else(|| MakiId::generate().hyphenated(), |s| s.id().hyphenated())
}

/// maki hands tools over in the Anthropic shape, which is what the endpoint
/// wants apart from the `type` discriminator.
fn convert_tools(tools: &Value) -> Vec<Value> {
    let Some(arr) = tools.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            Some(json!({
                "type": "function",
                "name": t.get("name")?.as_str()?,
                "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                "input_schema": t
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            }))
        })
        .collect()
}

fn image_part(source: &crate::types::ImageSource) -> Value {
    json!({
        "type": "image",
        "image": source.to_data_url(),
        "mimeType": source.media_type.mime(),
    })
}

/// Command Code rejects a tool call without its result and a result without its
/// call, so unpaired halves (an aborted turn, a trimmed history) are dropped
/// together rather than sent and 400'd.
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let calls: std::collections::HashMap<&str, &str> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    let paired: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. }
                if calls.contains_key(tool_use_id.as_str()) =>
            {
                Some(tool_use_id.as_str())
            }
            _ => None,
        })
        .collect();

    let mut out = Vec::new();
    for msg in messages {
        match msg.role {
            Role::Assistant => {
                // Reasoning is never replayed: the endpoint does not accept it
                // back on a later turn.
                let parts: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } if paired.contains(id.as_str()) => Some(json!({
                            "type": "tool-call",
                            "toolCallId": id,
                            "toolName": name,
                            "input": input,
                        })),
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    out.push(json!({"role": "assistant", "content": parts}));
                }
            }
            Role::User => {
                let mut results = Vec::new();
                let mut images = Vec::new();
                let mut plain = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } if paired.contains(tool_use_id.as_str()) => {
                            results.push(json!({
                                "type": "tool-result",
                                "toolCallId": tool_use_id,
                                "toolName": calls.get(tool_use_id.as_str()).copied().unwrap_or(""),
                                "output": {
                                    "type": if *is_error { "error-text" } else { "text" },
                                    "value": content,
                                },
                            }));
                        }
                        ContentBlock::Text { text } => {
                            plain.push(json!({"type": "text", "text": text}));
                        }
                        ContentBlock::Image { source } => images.push(image_part(source)),
                        _ => {}
                    }
                }
                if !results.is_empty() {
                    out.push(json!({"role": "tool", "content": results}));
                }
                // Images never ride along in a tool-result turn; they get their
                // own user turn, as the reference client does.
                plain.extend(images);
                if !plain.is_empty() {
                    out.push(json!({"role": "user", "content": plain}));
                }
            }
        }
    }
    out
}

// --- stream events ---

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputTokenDetails {
    no_cache_tokens: Option<u32>,
    cache_read_tokens: Option<u32>,
    cache_write_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    input_token_details: Option<InputTokenDetails>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcEvent {
    r#type: String,
    text: Option<String>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    input: Option<Value>,
    finish_reason: Option<String>,
    total_usage: Option<CcUsage>,
    error: Option<Value>,
    message: Option<Value>,
}

fn error_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn stop_reason_of(reason: &str) -> StopReason {
    match reason {
        "tool-calls" | "tool_calls" => StopReason::ToolUse,
        "length" | "max_tokens" | "max-tokens" | "max_output_tokens" => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}

fn apply_usage(usage: &mut TokenUsage, reported: &CcUsage) {
    let details = reported.input_token_details.as_ref();
    let cache_read = details.and_then(|d| d.cache_read_tokens).unwrap_or(0);
    let cache_write = details.and_then(|d| d.cache_write_tokens).unwrap_or(0);
    usage.input = details.and_then(|d| d.no_cache_tokens).unwrap_or_else(|| {
        // `inputTokens` is the all-in total, so the uncached part is what is
        // left after the cache halves; saturating so a provider inconsistency
        // cannot underflow into a huge number.
        reported
            .input_tokens
            .unwrap_or(0)
            .saturating_sub(cache_read)
            .saturating_sub(cache_write)
    });
    usage.output = reported.output_tokens.unwrap_or(0);
    usage.cache_read = cache_read;
    usage.cache_creation = cache_write;
}

/// Events arrive one JSON object per line, with or without an SSE `data:`
/// prefix. `None` for anything that is not an event (comments, `event:` lines,
/// keepalives, `[DONE]`).
fn parse_event_line(line: &str) -> Option<CcEvent> {
    let trimmed = line.trim();
    let payload = trimmed.strip_prefix("data:").unwrap_or(trimmed).trim();
    if payload.is_empty()
        || payload == "[DONE]"
        || trimmed.starts_with(':')
        || trimmed.starts_with("event:")
    {
        return None;
    }
    match serde_json::from_str(payload) {
        Ok(event) => Some(event),
        Err(e) => {
            // A drifted `finish` or `error` shape lands here; swallowing it
            // silently would surface later as a truncated turn instead.
            warn!(error = %e, "unparseable Command Code stream event");
            None
        }
    }
}

fn push_or_extend_text(blocks: &mut Vec<ContentBlock>, delta: &str) {
    if let Some(ContentBlock::Text { text }) = blocks.last_mut() {
        text.push_str(delta);
    } else {
        blocks.push(ContentBlock::Text { text: delta.into() });
    }
}

fn push_or_extend_thinking(blocks: &mut Vec<ContentBlock>, delta: &str) {
    if let Some(ContentBlock::Thinking { thinking, .. }) = blocks.last_mut() {
        thinking.push_str(delta);
    } else {
        blocks.push(ContentBlock::Thinking {
            thinking: delta.into(),
            signature: None,
        });
    }
}

async fn parse_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let mut lines = BufReader::new(response.into_body()).lines();
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut saw_finish = false;
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        let Some(event) = parse_event_line(&line) else {
            continue;
        };
        match event.r#type.as_str() {
            "text-delta" => {
                let Some(text) = event.text.filter(|t| !t.is_empty()) else {
                    continue;
                };
                event_tx
                    .send_async(ProviderEvent::TextDelta { text: text.clone() })
                    .await?;
                push_or_extend_text(&mut content, &text);
            }
            "reasoning-delta" => {
                let Some(text) = event.text.filter(|t| !t.is_empty()) else {
                    continue;
                };
                event_tx
                    .send_async(ProviderEvent::ThinkingDelta { text: text.clone() })
                    .await?;
                push_or_extend_thinking(&mut content, &text);
            }
            "tool-call" => {
                // An empty id or name would have the agent "execute" a tool
                // called "" and answer a call nothing is waiting on.
                let (Some(id), Some(name)) = (event.tool_call_id, event.tool_name) else {
                    warn!("Command Code tool-call event without an id or name, skipping");
                    continue;
                };
                event_tx
                    .send_async(ProviderEvent::ToolUseStart {
                        id: id.clone(),
                        name: name.clone(),
                    })
                    .await?;
                content.push(ContentBlock::ToolUse {
                    id,
                    name,
                    input: event.input.unwrap_or_else(|| json!({})),
                    thought_signature: None,
                });
                stop_reason = Some(StopReason::ToolUse);
            }
            "finish" => {
                if let Some(reported) = &event.total_usage {
                    apply_usage(&mut usage, reported);
                }
                if let Some(reason) = &event.finish_reason {
                    stop_reason = Some(stop_reason_of(reason));
                }
                saw_finish = true;
                break;
            }
            "error" => {
                let message = error_text(event.error.as_ref())
                    .or_else(|| error_text(event.message.as_ref()))
                    .unwrap_or_else(|| "Command Code stream error".into());
                // The endpoint reports mid-stream failures inside a 200, so
                // status is unavailable; 500 keeps them retryable.
                return Err(AgentError::Api {
                    status: 500,
                    message,
                });
            }
            // Everything the live stream also emits, none of it load-bearing
            // here: block boundaries are implied by the deltas, the assembled
            // `tool-call` supersedes the `tool-input-*` fragments that precede
            // it, and `finish-step` reports per-step usage that `finish`
            // totals. Listing them keeps the log below meaningful — an event
            // that reaches it is genuinely one this parser has never seen.
            "start" | "start-step" | "finish-step" | "text-start" | "text-end"
            | "reasoning-start" | "reasoning-end" | "tool-input-start" | "tool-input-delta"
            | "tool-input-end" | "tool-result" | "provider-metadata" => {}
            other => debug!(event = other, "unhandled Command Code stream event"),
        }
    }

    // Every complete turn ends in a `finish` event. Reaching EOF without one
    // means the connection dropped mid-answer, and a `None` stop reason would
    // be committed to history as a finished turn (`DoneReason::EndTurn`).
    // Retryable, so the turn is regenerated rather than silently truncated.
    if !saw_finish {
        return Err(AgentError::Api {
            status: 500,
            message: "Command Code stream ended without a finish event".into(),
        });
    }

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

impl Provider for CommandCode {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(self.do_stream(model, messages, system, tools, event_tx, opts, session_id))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        let url = format!("{}/provider/v1/models", self.base());
        let request = self.build_request("GET", &url).body(());
        let client = self.client.clone();
        Box::pin(async move {
            let mut response = client.send_async(request?).await?;
            if response.status().as_u16() != 200 {
                return Err(api_error(response).await);
            }
            super::parse_models(&response.text().await?)
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        super::adjust_model(model);
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let pool = resolve_key_pool(PLAN_SLUG)?;
            let base_url = self.base_url();
            *self.auth.lock().unwrap() = resolve_auth_from_key(pool.current(), base_url);
            Ok(())
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            let base_url = self.base_url();
            Ok(self.key_pool.as_ref().is_some_and(|p| {
                p.rotate_auth(&self.auth, |key| {
                    resolve_auth_from_key(key, base_url.clone())
                })
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::model;
    use super::*;

    fn provider() -> CommandCode {
        CommandCode {
            client: http_client(Timeouts::default()),
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer("k"))),
            key_pool: None,
            stream_timeout: Duration::from_secs(300),
        }
    }

    #[test]
    fn max_tokens_capped_at_generate_ceiling() {
        let body = provider().build_body(
            &model("claude-opus-5"),
            &[Message::user("hi".into())],
            "sys",
            &json!([]),
            RequestOptions::default(),
            "/tmp/proj",
            None,
        );
        assert_eq!(body["params"]["max_tokens"], MAX_GENERATE_TOKENS);
        assert_eq!(body["params"]["system"], "sys");
        assert_eq!(body["params"]["messages"][0]["role"], "user");
        assert_eq!(body["config"]["workingDir"], "/tmp/proj");
    }

    #[test]
    fn unpaired_tool_calls_and_results_are_dropped() {
        let messages = vec![
            Message::user("run it".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "sure".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "a".into(),
                        name: "bash".into(),
                        input: json!({"cmd": "ls"}),
                        thought_signature: None,
                    },
                    // Never answered: must not reach the wire.
                    ContentBlock::ToolUse {
                        id: "b".into(),
                        name: "bash".into(),
                        input: json!({}),
                        thought_signature: None,
                    },
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "a".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    // Answers a call that was never made.
                    ContentBlock::ToolResult {
                        tool_use_id: "zzz".into(),
                        content: "stale".into(),
                        is_error: false,
                    },
                ],
                ..Default::default()
            },
        ];

        let wire = convert_messages(&messages);
        let assistant = &wire[1];
        assert_eq!(assistant["content"].as_array().unwrap().len(), 2);
        assert_eq!(assistant["content"][1]["toolCallId"], "a");

        let tool = &wire[2];
        assert_eq!(tool["role"], "tool");
        let results = tool["content"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["toolCallId"], "a");
        assert_eq!(results[0]["toolName"], "bash");
        assert_eq!(results[0]["output"]["type"], "text");
    }

    #[test]
    fn thinking_is_not_replayed() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            ..Default::default()
        }];
        let wire = convert_messages(&messages);
        let parts = wire[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");
    }

    #[test]
    fn error_results_use_error_text_output() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "bash".into(),
                    input: json!({}),
                    thought_signature: None,
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "a".into(),
                    content: "boom".into(),
                    is_error: true,
                }],
                ..Default::default()
            },
        ];
        let wire = convert_messages(&messages);
        assert_eq!(wire[1]["content"][0]["output"]["type"], "error-text");
    }

    #[test]
    fn tools_gain_the_function_discriminator() {
        let tools = json!([{
            "name": "bash",
            "description": "run",
            "input_schema": {"type": "object", "properties": {}},
        }]);
        let converted = convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["name"], "bash");
        assert!(converted[0]["input_schema"]["properties"].is_object());
    }

    #[test]
    fn project_slug_matches_the_reference_derivation() {
        assert_eq!(
            project_slug("/Users/karl/Projects/maki"),
            "users-karl-projects-maki"
        );
        assert_eq!(project_slug("C:\\code\\My App"), "code-my-app");
        assert_eq!(project_slug("/"), "project");
        // Only a real drive letter is stripped; a colon deeper in a segment
        // must not eat the segment before it.
        assert_eq!(project_slug("/srv/mydir:file"), "srv-mydir-file");
    }

    #[test]
    fn thread_id_follows_the_session_so_the_endpoint_can_cache() {
        let session = SessionRef::from_id(MakiId::generate());
        assert_eq!(thread_id(Some(&session)), thread_id(Some(&session)));
        assert_eq!(thread_id(Some(&session)), session.id().hyphenated());

        // No session: a fresh hyphenated uuid, still what the endpoint wants.
        let id = thread_id(None);
        assert_eq!(id.len(), 36);
        assert_eq!(
            id.match_indices('-').map(|(i, _)| i).collect::<Vec<_>>(),
            vec![8, 13, 18, 23]
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(thread_id(None), thread_id(None));
    }

    #[test]
    fn live_event_vocabulary_is_all_accounted_for() {
        // Every event type observed from api.commandcode.ai on text, tool-call
        // and reasoning turns. A new one here means the protocol moved.
        let sse = concat!(
            "{\"type\":\"start\"}\n",
            "{\"type\":\"start-step\",\"warnings\":[]}\n",
            "{\"type\":\"reasoning-start\",\"id\":\"reasoning-0\"}\n",
            "{\"type\":\"reasoning-delta\",\"id\":\"reasoning-0\",\"text\":\"hmm\"}\n",
            "{\"type\":\"reasoning-end\",\"id\":\"reasoning-0\"}\n",
            "{\"type\":\"text-start\",\"id\":\"txt-0\"}\n",
            "{\"type\":\"text-delta\",\"id\":\"txt-0\",\"text\":\"OK\"}\n",
            "{\"type\":\"text-end\",\"id\":\"txt-0\"}\n",
            "{\"type\":\"tool-input-start\",\"id\":\"c1\",\"toolName\":\"bash\"}\n",
            "{\"type\":\"tool-input-delta\",\"id\":\"c1\",\"delta\":\"{\\\"command\\\":\"}\n",
            "{\"type\":\"tool-input-end\",\"id\":\"c1\"}\n",
            "{\"type\":\"tool-call\",\"toolCallId\":\"c1\",\"toolName\":\"bash\",",
            "\"input\":{\"command\":\"ls /tmp\"}}\n",
            "{\"type\":\"finish-step\",\"finishReason\":\"tool-calls\"}\n",
            "{\"type\":\"finish\",\"finishReason\":\"tool-calls\",\"totalUsage\":",
            "{\"inputTokens\":117,\"inputTokenDetails\":{\"noCacheTokens\":117,\"cacheReadTokens\":0},",
            "\"outputTokens\":36,\"totalTokens\":153}}\n",
        );
        let response = isahc::Response::builder()
            .status(200)
            .body(isahc::AsyncBody::from(sse))
            .unwrap();
        let (tx, rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(5))).unwrap();
        drop(tx);

        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        // The streaming tool-input fragments must not become a second call.
        assert_eq!(result.message.content.len(), 3);
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Thinking { thinking, .. } if thinking == "hmm"
        ));
        assert!(matches!(
            &result.message.content[1],
            ContentBlock::Text { text } if text == "OK"
        ));
        assert!(matches!(
            &result.message.content[2],
            ContentBlock::ToolUse { id, name, input, .. }
                if id == "c1" && name == "bash" && input["command"] == "ls /tmp"
        ));
        // Real payloads carry noCacheTokens, so the subtraction never runs.
        assert_eq!(result.usage.input, 117);
        assert_eq!(result.usage.output, 36);
        assert_eq!(rx.len(), 3);
    }

    #[test]
    fn http_errors_surface_the_message_not_the_envelope() {
        // Verbatim 403 body from api.commandcode.ai for an out-of-plan model.
        let body = r#"{"success":false,"error":{"code":"FORBIDDEN","status":403,"message":"MODEL_NOT_IN_PLAN: Gemini 3.5 Flash Lite available in Pro and above plans"}}"#;
        let response = isahc::Response::builder()
            .status(403)
            .body(isahc::AsyncBody::from(body))
            .unwrap();
        let err = smol::block_on(api_error(response));
        assert!(matches!(err, AgentError::Api { status: 403, .. }));
        assert_eq!(
            err.to_string(),
            "API error (403): MODEL_NOT_IN_PLAN: Gemini 3.5 Flash Lite available in Pro and above plans"
        );

        // A body that is not the documented envelope still reaches the user.
        let response = isahc::Response::builder()
            .status(502)
            .body(isahc::AsyncBody::from("upstream down"))
            .unwrap();
        assert!(
            smol::block_on(api_error(response))
                .to_string()
                .contains("upstream down")
        );
    }

    #[test]
    fn truncated_stream_is_an_error_not_a_finished_turn() {
        let sse = "data: {\"type\":\"text-delta\",\"text\":\"half an ans\"}\n";
        let response = isahc::Response::builder()
            .status(200)
            .body(isahc::AsyncBody::from(sse))
            .unwrap();
        let (tx, _rx) = flume::unbounded();
        let err = smol::block_on(parse_sse(response, &tx, Duration::from_secs(5))).unwrap_err();
        assert!(err.is_retryable());
        assert!(err.to_string().contains("without a finish event"));
    }

    #[test]
    fn tool_call_without_an_id_is_skipped() {
        let sse = concat!(
            "data: {\"type\":\"tool-call\",\"toolName\":\"bash\",\"input\":{}}\n",
            "data: {\"type\":\"finish\",\"finishReason\":\"stop\"}\n",
        );
        let response = isahc::Response::builder()
            .status(200)
            .body(isahc::AsyncBody::from(sse))
            .unwrap();
        let (tx, rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(5))).unwrap();
        drop(tx);
        assert!(result.message.content.is_empty());
        assert_eq!(rx.len(), 0);
    }

    #[test]
    fn parses_event_lines_with_and_without_sse_prefix() {
        assert_eq!(
            parse_event_line(r#"data: {"type":"text-delta","text":"hi"}"#)
                .unwrap()
                .text
                .unwrap(),
            "hi"
        );
        assert_eq!(
            parse_event_line(r#"{"type":"text-delta","text":"hi"}"#)
                .unwrap()
                .r#type,
            "text-delta"
        );
        assert!(parse_event_line("data: [DONE]").is_none());
        assert!(parse_event_line(": keepalive").is_none());
        assert!(parse_event_line("").is_none());
    }

    #[test]
    fn usage_falls_back_to_subtracting_cache_from_the_total() {
        let reported: CcUsage = serde_json::from_str(
            r#"{"inputTokens":1000,"outputTokens":50,
                "inputTokenDetails":{"cacheReadTokens":600,"cacheWriteTokens":100}}"#,
        )
        .unwrap();
        let mut usage = TokenUsage::default();
        apply_usage(&mut usage, &reported);
        assert_eq!(usage.input, 300);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.cache_read, 600);
        assert_eq!(usage.cache_creation, 100);

        // An explicit uncached count wins over the subtraction.
        let reported: CcUsage = serde_json::from_str(
            r#"{"inputTokens":1000,"outputTokens":1,
                "inputTokenDetails":{"noCacheTokens":250,"cacheReadTokens":600}}"#,
        )
        .unwrap();
        let mut usage = TokenUsage::default();
        apply_usage(&mut usage, &reported);
        assert_eq!(usage.input, 250);
    }

    #[test]
    fn stream_parses_text_tool_call_and_usage() {
        let sse = concat!(
            "data: {\"type\":\"text-delta\",\"text\":\"he\"}\n",
            "data: {\"type\":\"text-delta\",\"text\":\"llo\"}\n",
            "data: {\"type\":\"reasoning-delta\",\"text\":\"think\"}\n",
            "data: {\"type\":\"tool-call\",\"toolCallId\":\"c1\",\"toolName\":\"bash\",",
            "\"input\":{\"cmd\":\"ls\"}}\n",
            "data: {\"type\":\"finish\",\"finishReason\":\"tool-calls\",",
            "\"totalUsage\":{\"inputTokens\":10,\"outputTokens\":3}}\n",
        );
        let response = isahc::Response::builder()
            .status(200)
            .body(isahc::AsyncBody::from(sse))
            .unwrap();
        let (tx, rx) = flume::unbounded();
        let result = smol::block_on(parse_sse(response, &tx, Duration::from_secs(5))).unwrap();
        drop(tx);

        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(result.usage.input, 10);
        assert_eq!(result.usage.output, 3);
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Text { text } if text == "hello"
        ));
        assert!(matches!(
            &result.message.content[1],
            ContentBlock::Thinking { thinking, .. } if thinking == "think"
        ));
        assert!(matches!(
            &result.message.content[2],
            ContentBlock::ToolUse { id, name, .. } if id == "c1" && name == "bash"
        ));
        assert_eq!(rx.len(), 4);
    }

    #[test]
    fn mid_stream_error_event_becomes_a_retryable_api_error() {
        let sse = "data: {\"type\":\"error\",\"error\":{\"message\":\"upstream exploded\"}}\n";
        let response = isahc::Response::builder()
            .status(200)
            .body(isahc::AsyncBody::from(sse))
            .unwrap();
        let (tx, _rx) = flume::unbounded();
        let err = smol::block_on(parse_sse(response, &tx, Duration::from_secs(5))).unwrap_err();
        assert!(err.is_retryable());
        assert!(err.to_string().contains("upstream exploded"));
    }
}
