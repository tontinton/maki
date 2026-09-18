//! One-shot model calls. The agent loop is the wrong shape for "ask a model
//! one question": it drags in a system prompt, a tool set and a turn. This is
//! the seam plugins call through (`maki.model.complete`), and every call
//! reports what it spent so a host can bill it like any other model call.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use maki_providers::provider::{Provider, from_model};
use maki_providers::{
    ContentBlock, Message, Model, RequestOptions, Role, ThinkingConfig, Timeouts, TokenUsage,
};
use serde_json::Value;
use tracing::warn;

/// A one-shot call answers or it does not; without a ceiling a plugin that
/// fires one per tool call parks the caller on a hung provider.
pub const DEFAULT_COMPLETE_TIMEOUT_MS: u64 = 30_000;
/// Room for a sentence or two of answer. Callers that want an essay say so;
/// a model that emits reasoning tokens whatever you ask needs the room
/// raised, which is why this is a per-call knob and not one number for
/// models that behave nothing alike.
pub const DEFAULT_COMPLETE_MAX_OUTPUT_TOKENS: u32 = 1_024;

pub struct CompletionRequest {
    /// `provider/model-id`.
    pub spec: String,
    pub system: Option<String>,
    /// At least one message; the last is what the model answers.
    pub messages: Vec<Message>,
    pub max_output_tokens: u32,
    pub timeout_ms: u64,
}

/// What one call said and what it cost. `billed_cost` is what the account
/// pays, `list_cost` the un-subsidised price, both `None` on an unpriced
/// model, exactly as a turn reports them.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    /// Resolved model id, so a caller that passed an alias learns what ran.
    pub model: String,
    pub text: String,
    pub usage: TokenUsage,
    pub billed_cost: Option<f64>,
    pub list_cost: Option<f64>,
}

struct ResolvedModel {
    provider: Arc<dyn Provider>,
    model: Model,
}

/// Keyed by spec and output budget together: the budget is baked into the
/// resolved model, so two callers on one model with different budgets must
/// not share a handle. Process-wide, because a reviewer firing on every tool
/// call would otherwise re-resolve (and re-authenticate) each time.
type ResolvedCache = HashMap<(String, u32), Arc<ResolvedModel>>;

static RESOLVED: LazyLock<Mutex<ResolvedCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn resolve(
    spec: &str,
    timeouts: Timeouts,
    max_output_tokens: u32,
) -> Result<Arc<ResolvedModel>, String> {
    let mut cache = RESOLVED.lock().unwrap_or_else(|e| {
        warn!("completion model cache mutex was poisoned, recovering");
        e.into_inner()
    });
    let key = (spec.to_owned(), max_output_tokens);
    if let Some(hit) = cache.get(&key) {
        return Ok(Arc::clone(hit));
    }
    let mut model = Model::from_spec(spec).map_err(|e| e.to_string())?;
    model.max_output_tokens = Some(max_output_tokens);
    let provider = from_model(&mut model, timeouts).map_err(|e| e.to_string())?;
    let resolved = Arc::new(ResolvedModel {
        provider: Arc::from(provider),
        model,
    });
    cache.insert(key, Arc::clone(&resolved));
    Ok(resolved)
}

/// Ask {req.spec} once and hand back the text. Thinking is off and no tools
/// are offered: this is a question, not a turn.
pub async fn complete(req: CompletionRequest, timeouts: Timeouts) -> Result<Completion, String> {
    if req.messages.is_empty() {
        return Err("no messages to send".to_owned());
    }
    let resolved = resolve(&req.spec, timeouts, req.max_output_tokens)
        .map_err(|e| format!("model resolution: {e}"))?;
    let (event_tx, _event_rx) = flume::unbounded();
    let no_tools = Value::Array(Vec::new());
    let opts = RequestOptions {
        thinking: ThinkingConfig::Off,
        fast: false,
    }
    .clamped(&resolved.model);
    let system = req.system.unwrap_or_default();
    let call = resolved.provider.stream_message(
        &resolved.model,
        &req.messages,
        &system,
        &no_tools,
        &event_tx,
        opts,
        None,
    );
    let deadline = async {
        async_io::Timer::after(Duration::from_millis(req.timeout_ms)).await;
        Err(format!("timed out after {}ms", req.timeout_ms))
    };
    let response =
        futures_lite::future::or(async { call.await.map_err(|e| e.to_string()) }, deadline).await?;
    let text = response
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Completion {
        model: resolved.model.spec(),
        text,
        billed_cost: resolved.model.billed_cost(&response.usage, false),
        list_cost: resolved.model.list_cost(&response.usage, false),
        usage: response.usage,
    })
}

/// Builds the message list for the common shapes: a bare prompt, or rows of
/// `{role, content}`. Unknown roles are the caller's error, so they are
/// named rather than silently treated as user text.
pub fn message(role: &str, text: String) -> Result<Message, String> {
    let role = match role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        other => return Err(format!("unknown message role '{other}'")),
    };
    Ok(Message {
        role,
        content: vec![ContentBlock::Text { text }],
        ..Message::default()
    })
}
