use maki_providers::provider::Provider;
use maki_providers::retry::{MAX_TIMEOUT_RETRIES, RetryState};
use maki_providers::{
    Message, Model, ProviderEvent, RequestOptions, StopReason, StreamResponse, TokenUsage,
};
use serde_json::Value;
use tracing::{debug, warn};

use super::provider_hooks::{
    PROVIDER_HOOKS_TIMEOUT, ProviderHookSink, REQUEST_STAGE, RESPONSE_END_STAGE,
};
use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender};

async fn forward_provider_events(prx: flume::Receiver<ProviderEvent>, event_tx: &EventSender) {
    while let Ok(pe) = prx.recv_async().await {
        let ae = match pe {
            ProviderEvent::TextDelta { text } => AgentEvent::TextDelta { text },
            ProviderEvent::ThinkingDelta { text } => AgentEvent::ThinkingDelta { text },
            ProviderEvent::ToolUseStart { id, name } => AgentEvent::ToolPending { id, name },
        };
        if event_tx.send(ae).is_err() {
            break;
        }
    }
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
    session_id: Option<&str>,
    hooks: Option<&dyn ProviderHookSink>,
) -> Result<StreamResponse, AgentError> {
    let opts = opts.clamped(model);
    let provider_slug = model.provider.to_string();
    let slug = model.dynamic_slug.clone().unwrap_or(provider_slug);
    let mut retry = RetryState::new();
    loop {
        let (ptx, prx) = flume::unbounded();
        let (req_messages, req_system, req_tools) = if let Some(sink) = hooks {
            let ctx = serde_json::json!({
                "messages": messages,
                "system": system,
                "tools": tools,
            });
            match futures_lite::future::race(sink.run_hooks(REQUEST_STAGE, &slug, ctx), async {
                smol::Timer::after(PROVIDER_HOOKS_TIMEOUT).await;
                Err(AgentError::Timeout {
                    secs: PROVIDER_HOOKS_TIMEOUT.as_secs(),
                })
            })
            .await
            {
                Ok(transformed) => {
                    match extract_request_view(&transformed, messages, system, tools) {
                        Some(v) => {
                            debug!(stage = REQUEST_STAGE, slug = %slug, "provider hooks applied");
                            v
                        }
                        None => {
                            warn!(stage = REQUEST_STAGE, slug = %slug, "hooks returned invalid view; skipped");
                            (messages.to_vec(), system.to_owned(), tools.clone())
                        }
                    }
                }
                Err(e) => {
                    warn!(stage = REQUEST_STAGE, slug = %slug, error = %e, "hooks skipped");
                    (messages.to_vec(), system.to_owned(), tools.clone())
                }
            }
        } else {
            (messages.to_vec(), system.to_owned(), tools.clone())
        };
        let forwarder = smol::spawn({
            let event_tx = event_tx.clone();
            async move { forward_provider_events(prx, &event_tx).await }
        });
        let result = futures_lite::future::race(
            provider.stream_message(
                model,
                &req_messages,
                &req_system,
                &req_tools,
                &ptx,
                opts,
                session_id,
            ),
            async {
                cancel.cancelled().await;
                Err(AgentError::Cancelled)
            },
        )
        .await;
        drop(ptx);
        let _ = forwarder.await;
        match result {
            Ok(r) => {
                if let Some(sink) = hooks {
                    let ctx = serde_json::json!({
                        "message": r.message,
                        "usage": r.usage,
                        "stop_reason": r.stop_reason,
                    });
                    match futures_lite::future::race(
                        sink.run_hooks(RESPONSE_END_STAGE, &slug, ctx),
                        async {
                            smol::Timer::after(PROVIDER_HOOKS_TIMEOUT).await;
                            Err(AgentError::Timeout {
                                secs: PROVIDER_HOOKS_TIMEOUT.as_secs(),
                            })
                        },
                    )
                    .await
                    {
                        Ok(transformed) => match apply_response_hook(&transformed, r) {
                            Ok(out) => {
                                debug!(stage = RESPONSE_END_STAGE, slug = %slug, "provider hooks applied");
                                return Ok(out);
                            }
                            Err(original) => {
                                warn!(stage = RESPONSE_END_STAGE, slug = %slug, "hooks returned invalid view; skipped");
                                return Ok(original);
                            }
                        },
                        Err(e) => warn!(
                            stage = RESPONSE_END_STAGE,
                            slug = %slug,
                            error = %e,
                            "hooks skipped"
                        ),
                    }
                }
                return Ok(r);
            }
            Err(AgentError::Cancelled) => return Err(AgentError::Cancelled),
            Err(e) if e.is_retryable() => {
                if e.should_rotate_key()
                    && let Ok(true) = provider.rotate_key().await
                {
                    warn!("rotated API key after error: {e}");
                }
                let (attempt, delay) = retry.next_delay();
                if matches!(e, AgentError::Timeout { .. }) && attempt > MAX_TIMEOUT_RETRIES {
                    return Err(e);
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
                    return Err(AgentError::Cancelled);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

fn extract_request_view(
    transformed: &Value,
    messages: &[Message],
    system: &str,
    tools: &Value,
) -> Option<(Vec<Message>, String, Value)> {
    let out_messages = match transformed.get("messages") {
        Some(v) => serde_json::from_value::<Vec<Message>>(v.clone()).ok()?,
        None => messages.to_vec(),
    };
    let out_system = match transformed.get("system") {
        Some(v) => v.as_str().map(str::to_owned)?,
        None => system.to_owned(),
    };
    let out_tools = transformed
        .get("tools")
        .cloned()
        .unwrap_or_else(|| tools.clone());
    Some((out_messages, out_system, out_tools))
}

/// Applies a `response_end` hook's output to a `StreamResponse`. Absent
/// fields pass through unchanged; present-but-malformed fields cause the
/// whole transform to be rejected (`Err` carries back the original).
/// Mirrors `extract_request_view`'s fail-loud-on-malformed semantics.
fn apply_response_hook(
    transformed: &Value,
    mut r: StreamResponse,
) -> Result<StreamResponse, StreamResponse> {
    if let Some(v) = transformed.get("message") {
        match serde_json::from_value::<Message>(v.clone()) {
            Ok(m) => r.message = m,
            Err(_) => return Err(r),
        }
    }
    if let Some(v) = transformed.get("usage") {
        match serde_json::from_value::<TokenUsage>(v.clone()) {
            Ok(u) => r.usage = u,
            Err(_) => return Err(r),
        }
    }
    if let Some(v) = transformed.get("stop_reason") {
        match serde_json::from_value::<StopReason>(v.clone()) {
            Ok(s) => r.stop_reason = Some(s),
            Err(_) => return Err(r),
        }
    }
    Ok(r)
}
