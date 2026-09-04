use std::time::{Duration, Instant};

use maki_providers::provider::Provider;
use maki_providers::retry::{MAX_TIMEOUT_RETRIES, RetryState};
use maki_providers::{ContentBlock, Message, Model, ProviderEvent, RequestOptions, StreamResponse};
use maki_storage::id::SessionRef;
use serde_json::Value;
use tracing::warn;

use super::run::estimate_prompt_tokens;
use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender};

const FUNCTIONS_PREFIX: &str = "functions.";
/// Floor for the clamp below, never applied above the model's own cap.
/// Anthropic derives the thinking budget from `max_tokens` (half of it, at
/// least 1024) and rejects a request where the two are equal, so clamping the
/// cap all the way down to 1024 would trade an overflow for a 400.
const MIN_OUTPUT_TOKENS: u32 = 4096;

/// GPT models sometimes emit `functions.<name>`, a Codex training habit.
/// Stripped here at the provider boundary so no raw name enters the agent;
/// the batch plugin mirrors the rule in Lua.
pub(crate) fn canonical_tool_name(name: &str) -> &str {
    name.strip_prefix(FUNCTIONS_PREFIX).unwrap_or(name)
}

fn canonicalize_tool_names(message: &mut Message) {
    for block in &mut message.content {
        if let ContentBlock::ToolUse { name, .. } = block {
            *name = canonical_tool_name(name).to_owned();
        }
    }
}

async fn forward_provider_events(
    prx: flume::Receiver<ProviderEvent>,
    event_tx: &EventSender,
) -> String {
    let mut streamed = String::new();
    while let Ok(pe) = prx.recv_async().await {
        let ae = match pe {
            ProviderEvent::TextDelta { text } => {
                streamed.push_str(&text);
                AgentEvent::TextDelta { text }
            }
            ProviderEvent::ThinkingDelta { text } => AgentEvent::ThinkingDelta { text },
            ProviderEvent::ToolUseStart { id, name } => AgentEvent::ToolPending {
                id,
                name: canonical_tool_name(&name).to_owned(),
            },
            ProviderEvent::PromptProgress {
                processed,
                total,
                cache,
            } => AgentEvent::PromptProgress {
                processed,
                total,
                cache,
            },
        };
        if event_tx.send(ae).is_err() {
            break;
        }
    }
    streamed
}

/// Cancelling mid-stream carries the text the user still sees on screen,
/// so the caller can keep it in history. A cancel during the retry backoff
/// carries nothing: the `Retry` event already made the view drop the failed
/// attempt's text (`stream_reset`), and history must agree with the view.
#[derive(Debug)]
pub(crate) enum StreamError {
    Cancelled { streamed: String },
    Other(AgentError),
}

impl From<AgentError> for StreamError {
    fn from(e: AgentError) -> Self {
        Self::Other(e)
    }
}

impl From<StreamError> for AgentError {
    fn from(e: StreamError) -> Self {
        match e {
            StreamError::Cancelled { .. } => Self::Cancelled,
            StreamError::Other(e) => e,
        }
    }
}

/// Servers like vLLM reject `prompt + max_tokens > window` even when the
/// prompt alone fits. Returns the reduced cap, or `None` when the model's own
/// cap already fits.
fn clamped_output_tokens(model: &Model, prompt_tokens: u32) -> Option<u32> {
    let max_output = model.max_output_tokens?;
    let remaining = model.context_window.saturating_sub(prompt_tokens);
    // The `min` keeps the floor from raising the cap over what the model
    // declared, since it would reject a number it never offered.
    (max_output > remaining).then(|| remaining.max(MIN_OUTPUT_TOKENS).min(max_output))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_with_retry(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    event_tx: &EventSender,
    cancel: &CancelToken,
    opts: RequestOptions,
    session_id: Option<&SessionRef>,
) -> Result<StreamResponse, StreamError> {
    let opts = opts.clamped(model);
    let prompt_tokens = estimate_prompt_tokens(messages, system, tools);
    let fitted = clamped_output_tokens(model, prompt_tokens).map(|max_output_tokens| {
        warn!(
            model = %model.id,
            prompt_tokens,
            context_window = model.context_window,
            max_output_tokens,
            "clamped max output tokens to fit the context window"
        );
        Model {
            max_output_tokens: Some(max_output_tokens),
            ..model.clone()
        }
    });
    let model = fitted.as_ref().unwrap_or(model);
    let messages = maki_providers::adapt_images_for_model(model, messages);
    let messages = &*messages;
    let mut retry = RetryState::new();
    loop {
        let started = Instant::now();
        let (ptx, prx) = flume::unbounded();
        let forwarder = smol::spawn({
            let event_tx = event_tx.clone();
            async move { forward_provider_events(prx, &event_tx).await }
        });
        let result = futures_lite::future::race(
            provider.stream_message(model, messages, system, tools, &ptx, opts, session_id),
            async {
                cancel.cancelled().await;
                Err(AgentError::Cancelled)
            },
        )
        .await;
        drop(ptx);
        let streamed = forwarder.await;
        match result {
            Ok(mut r) => {
                canonicalize_tool_names(&mut r.message);
                emit_api_request(model, &r, opts, started.elapsed());
                return Ok(r);
            }
            Err(AgentError::Cancelled) => return Err(StreamError::Cancelled { streamed }),
            Err(e) if e.is_retryable() => {
                emit_api_error(model, &e, retry.attempts() + 1, started.elapsed());
                if e.should_rotate_key()
                    && let Ok(true) = provider.rotate_key().await
                {
                    warn!("rotated API key after error: {e}");
                }
                let (attempt, delay) = retry.next_delay();
                if matches!(e, AgentError::Timeout { .. }) && attempt > MAX_TIMEOUT_RETRIES {
                    return Err(e.into());
                }
                let delay_ms = delay.as_millis() as u64;
                warn!(attempt, delay_ms, error = %e, "retryable, will retry");
                event_tx.send(AgentEvent::Retry {
                    attempt,
                    message: e.retry_message(),
                    delay_ms,
                })?;
                futures_lite::future::race(
                    async {
                        smol::Timer::after(delay).await;
                    },
                    cancel.cancelled(),
                )
                .await;
                if cancel.is_cancelled() {
                    return Err(StreamError::Cancelled {
                        streamed: String::new(),
                    });
                }
            }
            Err(e) => {
                emit_api_error(model, &e, retry.attempts() + 1, started.elapsed());
                return Err(e.into());
            }
        }
    }
}

fn emit_api_request(model: &Model, r: &StreamResponse, opts: RequestOptions, took: Duration) {
    if !maki_otel::enabled() {
        return;
    }
    let usage = &r.usage;
    maki_otel::emit::api_request(&maki_otel::emit::ApiRequest {
        model: &model.id,
        provider: &model.provider,
        input_tokens: u64::from(usage.input),
        output_tokens: u64::from(usage.output),
        cache_read_tokens: u64::from(usage.cache_read),
        cache_creation_tokens: u64::from(usage.cache_creation),
        cost_usd: model.billed_cost(usage, opts.fast).unwrap_or(0.0),
        duration: took,
        stop_reason: r.stop_reason.map(<&'static str>::from),
    });
}

fn emit_api_error(model: &Model, error: &AgentError, attempt: u32, took: Duration) {
    if !maki_otel::enabled() {
        return;
    }
    maki_otel::emit::api_error(&maki_otel::emit::ApiError {
        model: &model.id,
        provider: &model.provider,
        error: &error_description(error),
        status_code: match error {
            AgentError::Api { status, .. } => Some(*status),
            _ => None,
        },
        attempt,
        duration: took,
    });
}

/// A provider's error body is often echoed request content (quoted message
/// text, masked keys, whatever a gateway returns), so only the status is
/// reported. Every other variant is generated locally.
fn error_description(error: &AgentError) -> String {
    match error {
        AgentError::Api { status, .. } => format!("API error ({status})"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use maki_providers::Role;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const SECRET_BODY: &str = "messages.0.content: \"my private prompt\", key sk-abc";
    const WINDOW: u32 = 262_144;
    const BIG_MAX_OUTPUT: u32 = 100_000;
    const SMALL_MAX_OUTPUT: u32 = 2_048;
    const SMALL_PROMPT: u32 = 1_000;
    /// One token more than the window can spare for `BIG_MAX_OUTPUT`.
    const CROWDING_PROMPT: u32 = WINDOW - BIG_MAX_OUTPUT + 1;

    fn model_with(max_output_tokens: Option<u32>) -> Model {
        let mut model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        model.context_window = WINDOW;
        model.max_output_tokens = max_output_tokens;
        model
    }

    #[test_case(Some(BIG_MAX_OUTPUT), SMALL_PROMPT, None; "cap_fits_inside_remaining_window")]
    #[test_case(Some(BIG_MAX_OUTPUT), CROWDING_PROMPT, Some(WINDOW - CROWDING_PROMPT); "cap_exceeds_remaining_window")]
    #[test_case(Some(BIG_MAX_OUTPUT), WINDOW + 1, Some(MIN_OUTPUT_TOKENS); "prompt_over_window_floors_at_minimum")]
    #[test_case(Some(SMALL_MAX_OUTPUT), WINDOW + 1, Some(SMALL_MAX_OUTPUT); "floor_never_exceeds_the_model_cap")]
    #[test_case(None, CROWDING_PROMPT, None; "provider_chosen_cap_is_left_alone")]
    fn clamps_output_tokens_to_the_remaining_window(
        max_output_tokens: Option<u32>,
        prompt_tokens: u32,
        expected: Option<u32>,
    ) {
        assert_eq!(
            clamped_output_tokens(&model_with(max_output_tokens), prompt_tokens),
            expected
        );
    }

    #[test]
    fn tool_use_names_canonicalized() {
        let mut message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "hi".into() },
                ContentBlock::tool_use("t1", "functions.bash", json!({})),
                ContentBlock::tool_use("t2", "read", json!({})),
                ContentBlock::tool_use("t3", "my_functions.x", json!({})),
            ],
            ..Default::default()
        };
        canonicalize_tool_names(&mut message);
        let names: Vec<&str> = message.tool_uses().map(|(_, name, _)| name).collect();
        assert_eq!(names, ["bash", "read", "my_functions.x"]);
    }

    #[test]
    fn a_reported_api_error_leaves_the_provider_body_behind() {
        let error = AgentError::Api {
            status: 400,
            message: SECRET_BODY.into(),
        };
        let reported = error_description(&error);
        assert!(!reported.contains("private"));
        assert_eq!(reported, "API error (400)");
    }
}
