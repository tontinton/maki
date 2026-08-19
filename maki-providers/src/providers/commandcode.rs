//! Command Code token-plan provider (GOAT / Pro / Max / Team).
//!
//! Token plans do not speak the OpenAI-compatible `/provider/v1/chat/completions`
//! endpoint the pay-as-you-go Provider plan uses. They speak a custom SSE
//! protocol at `POST {base}/alpha/generate`: a request envelope of
//! `config`/`memory`/`taste`/`skills`/`params`/`threadId`, and a Vercel-AI-SDK
//! shaped event stream (`text-delta`, `reasoning-delta`, `tool-call`, `finish`).
//! Wire format reverse-engineered from `pi-commandcode-provider` against
//! command-code CLI 1.15.1; the model catalog at `/provider/v1/models` is the
//! only OpenAI-shaped part.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::Effort;
use maki_storage::sessions::Effort::{High, Low, Max, Medium, XHigh};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{Model, ModelEntry, ModelInfo, ThinkingSupport};
use crate::provider::{BoxFuture, Provider};
use crate::types::EffortDialect;
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, RequestOptions, Role, StopReason,
    StreamResponse, ThinkingConfig, TokenUsage,
};

use super::{KeyPool, ResolvedAuth, http_client, next_sse_line, user_agent};

const SLUG: &str = "command-code";
const BASE_URL: &str = "https://api.commandcode.ai";
const ENV_VAR: &str = "COMMAND_CODE_API_KEY";
/// Sent as `x-command-code-version`; the endpoint gates behaviour on it.
const CLI_VERSION: &str = "1.15.1";
/// The generate endpoint's own ceiling, independent of the model window.
const MAX_GENERATE_TOKENS: u32 = 64_000;
const DEFAULT_TEMPERATURE: f64 = 0.3;

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: SLUG,
    display_name: "Command Code",
    protocol: maki_config::providers::Protocol::CommandCode,
    default_base_url: BASE_URL,
    default_api_key_env: ENV_VAR,
    default_model: "command-code/claude-opus-5",
    plans: None,
    login_url: Some("https://commandcode.ai"),
    needs_url: false,
});

/// Empty on purpose: the catalog is fetched from `/provider/v1/models`, and a
/// token plan bills against the subscription, so static per-token pricing here
/// would only be a second source of truth to keep in sync.
pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}

const FULL: &[Effort] = &[Low, Medium, High, XHigh, Max];
const TO_XHIGH: &[Effort] = &[Low, Medium, High, XHigh];
const TO_HIGH: &[Effort] = &[Low, Medium, High];
const HIGH_MAX: &[Effort] = &[High, Max];
const HIGH_XHIGH: &[Effort] = &[High, XHigh];
const NONE: &[Effort] = &[];

/// `(model id, accepted reasoning efforts, accepts image input)`.
///
/// `/provider/v1/models` returns only id/name/context_length, so reasoning and
/// vision have to come from somewhere: this is a snapshot of the
/// command-code@1.15.1 bundled catalog. An id missing here is treated as
/// text-only with provider-chosen reasoning depth, which is what the CLI does
/// too, so a newly released model degrades instead of erroring.
///
/// ponytail: hand-maintained snapshot. Refresh from
/// <https://commandcode.ai/docs/resources/pricing-limits> when models land; if
/// the catalog endpoint ever exposes these fields, delete the table.
const CATALOG: &[(&str, &[Effort], bool)] = &[
    ("MiniMaxAI/MiniMax-M3", NONE, true),
    ("Qwen/Qwen3.6-Plus", NONE, true),
    ("Qwen/Qwen3.7-Flash", NONE, true),
    ("Qwen/Qwen3.7-Plus", NONE, true),
    ("Qwen/Qwen3.8-Max", &[Low, Medium, XHigh], true),
    ("claude-fable-5", FULL, true),
    ("claude-haiku-4-5-20251001", NONE, true),
    ("claude-opus-4-7", FULL, true),
    ("claude-opus-4-8", FULL, true),
    ("claude-opus-5", FULL, true),
    ("claude-sonnet-4-6", FULL, true),
    ("claude-sonnet-5", FULL, true),
    ("deepseek/deepseek-v4-flash", HIGH_MAX, false),
    ("deepseek/deepseek-v4-pro", HIGH_MAX, false),
    ("google/gemini-3.1-flash-lite", TO_HIGH, true),
    ("google/gemini-3.5-flash", TO_HIGH, true),
    ("google/gemini-3.5-flash-lite", TO_HIGH, true),
    ("google/gemini-3.6-flash", TO_HIGH, true),
    ("gpt-5.3-codex", TO_XHIGH, true),
    ("gpt-5.4", TO_XHIGH, true),
    ("gpt-5.4-mini", TO_HIGH, true),
    ("gpt-5.5", TO_XHIGH, true),
    ("gpt-5.6-luna", FULL, true),
    ("gpt-5.6-sol", FULL, true),
    ("gpt-5.6-terra", FULL, true),
    ("meta/muse-spark-1.1", NONE, true),
    ("meta/muse-spark-1.2", NONE, true),
    ("meta/muse-spark-1.2-contributor", NONE, true),
    ("moonshotai/Kimi-K2.5", NONE, true),
    ("moonshotai/Kimi-K2.6", NONE, true),
    ("moonshotai/Kimi-K2.7-Code", NONE, true),
    ("moonshotai/Kimi-K2.7-Code-Highspeed", NONE, true),
    ("moonshotai/Kimi-K3", NONE, true),
    ("sakana/fugu-ultra", HIGH_XHIGH, true),
    ("stepfun/Step-3.7-Flash", NONE, true),
    ("thinkingmachines/inkling", NONE, true),
    ("thinkingmachines/inkling-small", NONE, true),
    ("xai/grok-4.5", TO_HIGH, true),
    ("xiaomi/mimo-v2.5", NONE, true),
    ("zai-org/GLM-5.2", HIGH_MAX, false),
];

fn catalog_entry(model_id: &str) -> Option<&'static (&'static str, &'static [Effort], bool)> {
    CATALOG.iter().find(|(id, _, _)| *id == model_id)
}

/// `None` means send no `reasoning_effort` and let Command Code pick, which is
/// also what an unknown model gets.
fn reasoning_effort(model: &Model, thinking: ThinkingConfig) -> Option<&'static str> {
    let (_, efforts, _) = catalog_entry(&model.id)?;
    if efforts.is_empty() {
        return None;
    }
    thinking.effort_str(
        &EffortDialect {
            supported: efforts,
            // Command Code has no adaptive level and no explicit opt-out
            // string: both mean "omit the field".
            adaptive: None,
            off: None,
        },
        model,
    )
}

/// Credential files written by the Command Code CLI and by pi/omp hosts. maki's
/// own `KeyPool` (env, `maki auth login`, providers.toml) is tried first; this
/// is the fallback that makes an existing CLI login just work.
fn key_from_cli_files() -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let paths = [
        home.join(".commandcode/auth.json"),
        home.join(".omp/agent/auth.json"),
        home.join(".pi/agent/auth.json"),
    ];
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let direct = ["apiKey", "commandcode"]
            .iter()
            .find_map(|k| root.get(*k)?.as_str());
        if let Some(key) = direct {
            return Some(key.to_string());
        }
        // `{"command-code": {"type":"api","key":"..."}}` from the CLI, or the
        // same shape with `type: "oauth"` and an `access` token.
        let nested = ["commandcode", "command-code"].iter().find_map(|k| {
            let record = root.get(*k)?;
            record.get("key").or_else(|| record.get("access"))?.as_str()
        });
        if let Some(key) = nested {
            return Some(key.to_string());
        }
    }
    None
}

fn resolve_key_pool() -> Result<KeyPool, AgentError> {
    match KeyPool::resolve(SLUG, ENV_VAR) {
        Ok(pool) => Ok(pool),
        Err(e) => key_from_cli_files().map_or(Err(e), |key| Ok(KeyPool::from_keys(vec![key]))),
    }
}

fn resolve_auth_from_key(key: &str, base_url: Option<String>) -> ResolvedAuth {
    let mut auth = ResolvedAuth::bearer(key);
    auth.base_url = base_url;
    auth
}

pub struct CommandCode {
    client: HttpClient,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    stream_timeout: Duration,
}

impl CommandCode {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = resolve_key_pool()?;
        let config = maki_config::providers::ProvidersConfig::load();
        let base_url = maki_config::providers::resolve_base_url(SLUG, config.get(SLUG));
        Ok(Self {
            client: http_client(timeouts),
            auth: Arc::new(Mutex::new(resolve_auth_from_key(pool.current(), base_url))),
            key_pool: Some(pool),
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
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

    fn build_body(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        opts: RequestOptions,
        working_dir: &str,
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
            "threadId": thread_id(),
        })
    }

    async fn do_stream(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        opts: RequestOptions,
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
            Err(AgentError::from_response(response).await)
        }
    }
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

/// A fresh RFC 4122 string per request, matching the reference client. The
/// endpoint treats it as an opaque conversation key.
///
/// ponytail: per-request id means no server-side thread reuse. Thread it from
/// `SessionRef` if Command Code turns out to cache across a thread.
fn thread_id() -> String {
    MakiId::generate().hyphenated()
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
            // reasoning-start/reasoning-end/tool-result carry no content maki
            // needs; the block boundaries are implied by the deltas. Anything
            // else is logged, not dropped silently: the protocol is
            // reverse-engineered and may grow events that matter.
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

// --- model catalog ---

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    context_length: Option<u32>,
}

#[derive(Deserialize)]
struct CatalogResponse {
    data: Vec<CatalogModel>,
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
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(self.do_stream(model, messages, system, tools, event_tx, opts))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        let url = format!("{}/provider/v1/models", self.base());
        let request = self.build_request("GET", &url).body(());
        let client = self.client.clone();
        Box::pin(async move {
            let mut response = client.send_async(request?).await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }
            let body = response.text().await?;
            let catalog: CatalogResponse = serde_json::from_str(&body)?;
            let mut infos: Vec<ModelInfo> = catalog
                .data
                .into_iter()
                .map(|m| {
                    let entry = catalog_entry(&m.id);
                    ModelInfo {
                        context_window: m.context_length,
                        // The catalog exposes no output ceiling, so the context
                        // window stands in for it, as the reference client
                        // does. A model whose real cap is under 64k would be
                        // asked for more than it allows.
                        max_output_tokens: Some(
                            m.context_length
                                .unwrap_or(MAX_GENERATE_TOKENS)
                                .min(MAX_GENERATE_TOKENS),
                        ),
                        supports_thinking: entry.map(|(_, efforts, _)| !efforts.is_empty()),
                        supports_vision: entry.map(|(_, _, vision)| *vision),
                        ..ModelInfo::id_only(m.id)
                    }
                })
                .collect();
            infos.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(infos)
        })
    }

    /// Discovery has not necessarily run when the first request goes out, and
    /// until it does the Generic manifest answers "no vision, thinking on
    /// everything" — which silently strips images from the first turn on a
    /// vision model. Seed both from the catalog snapshot; unknown ids fall
    /// through to discovery unchanged.
    fn adjust_model(&self, model: &mut Model) {
        let Some((_, efforts, vision)) = catalog_entry(&model.id) else {
            return;
        };
        model.supports_vision_override = Some(*vision);
        model.thinking_override = Some(if efforts.is_empty() {
            ThinkingSupport::No
        } else {
            ThinkingSupport::Yes
        });
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let pool = resolve_key_pool()?;
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
    use super::*;
    use crate::model::{ModelFamily, ModelPricing, ModelTier};

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            provider: Arc::from(SLUG),
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            pricing: ModelPricing::default(),
            max_output_tokens: Some(200_000),
            context_window: 400_000,
            discovered_free: false,
            thinking_fields: None,
        }
    }

    fn provider() -> CommandCode {
        CommandCode {
            client: http_client(super::super::Timeouts::default()),
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
        );
        assert_eq!(body["params"]["max_tokens"], MAX_GENERATE_TOKENS);
        assert_eq!(body["params"]["system"], "sys");
        assert_eq!(body["params"]["messages"][0]["role"], "user");
        assert_eq!(body["config"]["workingDir"], "/tmp/proj");
    }

    #[test]
    fn effort_snaps_to_what_the_model_accepts() {
        // deepseek accepts only high/max, so Low must not go out as "low".
        assert_eq!(
            reasoning_effort(
                &model("deepseek/deepseek-v4-pro"),
                ThinkingConfig::Effort(Low)
            ),
            Some("high"),
        );
        assert_eq!(
            reasoning_effort(&model("claude-opus-5"), ThinkingConfig::Effort(XHigh)),
            Some("xhigh"),
        );
        // Non-reasoning and unknown models send nothing at all.
        assert_eq!(
            reasoning_effort(&model("moonshotai/Kimi-K3"), ThinkingConfig::Effort(High)),
            None,
        );
        assert_eq!(
            reasoning_effort(&model("brand/new-model"), ThinkingConfig::Effort(High)),
            None,
        );
        assert_eq!(
            reasoning_effort(&model("claude-opus-5"), ThinkingConfig::Off),
            None,
        );
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
    fn thread_id_is_a_hyphenated_uuid() {
        let id = thread_id();
        assert_eq!(id.len(), 36);
        assert_eq!(
            id.match_indices('-').map(|(i, _)| i).collect::<Vec<_>>(),
            vec![8, 13, 18, 23]
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(thread_id(), thread_id());
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
    fn catalog_capabilities_apply_before_discovery_warms() {
        let cc = provider();
        let mut vision_model = model("claude-opus-5");
        cc.adjust_model(&mut vision_model);
        assert!(vision_model.supports_vision());
        assert!(vision_model.supports_thinking());

        let mut plain = model("moonshotai/Kimi-K3");
        cc.adjust_model(&mut plain);
        assert!(plain.supports_vision());
        assert!(!plain.supports_thinking());

        // Unknown ids stay untouched so discovery can still fill them in.
        let mut unknown = model("brand/new-model");
        cc.adjust_model(&mut unknown);
        assert!(unknown.supports_vision_override.is_none());
        assert!(unknown.thinking_override.is_none());
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
