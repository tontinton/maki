use std::env;

use maki_providers::{ContentBlock, Message, Model, RequestOptions, StreamResponse, TokenUsage};
use tracing::{info, warn};

use super::history::{CutPoint, History};
use super::streaming::stream_with_retry;
use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender, TurnCompleteEvent};

pub(super) const CONTINUE_AFTER_COMPACT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed. If you learned important project context during this session, consider saving it to memory before it's lost.";
const IMAGE_PLACEHOLDER: &str = "[image]";

pub(super) async fn compact_history(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<TokenUsage, AgentError> {
    let compact_start = std::time::Instant::now();

    let Some(cut) = history.compaction_cut(KEEP_RECENT_TOKENS) else {
        warn!("compaction refused: nothing eligible or leaf is a compaction");
        return Ok(TokenUsage::default());
    };

    let mut prefix = history.compaction_prefix(&cut);
    if prefix.is_empty() {
        warn!("compaction refused: empty prefix");
        return Ok(TokenUsage::default());
    }
    strip_images(&mut prefix);
    strip_thinking(&mut prefix);
    prefix.push(Message::user(crate::prompt::COMPACTION_USER.to_string()));

    let empty_tools = serde_json::json!([]);
    let max_attempts = 3;
    let mut last_error = None;

    for attempt in 0..max_attempts {
        match stream_with_retry(
            provider,
            model,
            &prefix,
            crate::prompt::COMPACTION_SYSTEM,
            &empty_tools,
            event_tx,
            cancel,
            RequestOptions::default(),
            None,
        )
        .await
        {
            Ok(response) => {
                if attempt > 0 {
                    info!(
                        attempt,
                        "compaction succeeded after truncating oldest rounds"
                    );
                }
                return Ok(finish_compact(
                    response,
                    history,
                    cut,
                    event_tx,
                    compact_start,
                    model,
                ));
            }
            Err(e) if e.is_context_overflow() && attempt < max_attempts - 1 => {
                last_error = Some(e);
                truncate_oldest_round(&mut prefix);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_error.unwrap())
}

/// Generate a branch-summary narrative for the abandoned branch (§6), then
/// freeze it into a `SummaryRecord` with `SummaryKind::Branch`. Mirrors
/// `compact_history` but summarizes the abandoned off-path branch rather than
/// the compaction cut. On cancel, returns `AgentError::Cancelled` and no record
/// is appended (rewind proceeds clean, §6).
pub async fn branch_summary(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    parent: maki_storage::tree::NodeRef,
    fold_from_id: maki_storage::tree::NodeRef,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<TokenUsage, AgentError> {
    let summary_start = std::time::Instant::now();

    let mut prefix = history.abandoned_branch_prefix(&parent, &fold_from_id);
    if prefix.is_empty() {
        warn!("branch-summary refused: empty abandoned branch");
        return Ok(TokenUsage::default());
    }
    strip_images(&mut prefix);
    strip_thinking(&mut prefix);
    prefix.push(Message::user(crate::prompt::COMPACTION_USER.to_string()));

    let empty_tools = serde_json::json!([]);
    let response = stream_with_retry(
        provider,
        model,
        &prefix,
        crate::prompt::COMPACTION_SYSTEM,
        &empty_tools,
        event_tx,
        cancel,
        RequestOptions::default(),
        None,
    )
    .await?;

    let _ = event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        context_size: Some(response.usage.output),
    })));

    let narrative = response
        .message
        .first_text_content()
        .unwrap_or_default()
        .to_owned();

    history.append_branch_summary(parent, fold_from_id, narrative, CONTINUE_AFTER_COMPACT);

    info!(
        model = %model.id,
        duration_ms = summary_start.elapsed().as_millis() as u64,
        "branch-summary completed"
    );

    Ok(response.usage)
}

fn finish_compact(
    response: StreamResponse,
    history: &mut History,
    cut: CutPoint,
    event_tx: &EventSender,
    compact_start: std::time::Instant,
    model: &Model,
) -> TokenUsage {
    let _ = event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        context_size: Some(response.usage.output),
    })));

    let narrative = response
        .message
        .first_text_content()
        .unwrap_or_default()
        .to_owned();

    history.append_compaction(
        cut,
        narrative,
        CONTINUE_AFTER_COMPACT,
        Vec::new(),
        Vec::new(),
    );

    info!(
        model = %model.id,
        duration_ms = compact_start.elapsed().as_millis() as u64,
        "compaction completed"
    );

    response.usage
}

pub async fn compact(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
) -> Result<(), AgentError> {
    let cancel = CancelToken::none();
    let usage = compact_history(provider, model, history, event_tx, &cancel).await?;

    event_tx.send(AgentEvent::Done {
        usage,
        num_turns: 1,
        stop_reason: None,
    })?;

    Ok(())
}

pub(super) fn is_overflow(usage: &TokenUsage, model: &Model, compaction_buffer: u32) -> bool {
    let usable = model.context_window.saturating_sub(compaction_buffer);
    usage.context_tokens() >= usable
}

fn strip_images(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            if matches!(block, ContentBlock::Image { .. }) {
                *block = ContentBlock::Text {
                    text: IMAGE_PLACEHOLDER.into(),
                };
            }
        }
    }
}

fn strip_thinking(messages: &mut [Message]) {
    for msg in messages {
        msg.content.retain(|block| {
            !matches!(
                block,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        });
    }
}

/// Approximate keep-recent budget for cut selection (§6). Token estimate is
/// char-based, matching `estimate_message_tokens` in run.rs.
const KEEP_RECENT_TOKENS: u32 = 20_000;

fn truncate_oldest_round(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }

    let mut remove_count = 1;

    if matches!(
        messages.first().map(|m| &m.role),
        Some(maki_providers::Role::Assistant)
    ) {
        let has_tool_calls = messages[0].has_tool_calls();
        if has_tool_calls {
            let next_has_tool_results = messages.get(1).is_some_and(|m| {
                matches!(m.role, maki_providers::Role::User)
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            });
            if next_has_tool_results {
                remove_count = 2;
            }
        }
    } else if matches!(
        messages.first().map(|m| &m.role),
        Some(maki_providers::Role::User)
    ) && matches!(
        messages.get(1).map(|m| &m.role),
        Some(maki_providers::Role::Assistant)
    ) {
        // Dropping a lone user message would leave assistant-first, which some providers reject.
        // Remove the assistant too to keep the conversation well-formed.
        remove_count = 2;
    }

    messages.drain(..remove_count);

    // After draining, the first message might still be an assistant (e.g. consecutive
    // assistant messages). Keep draining until the first message is user or we're empty.
    while messages.len() > 1
        && matches!(
            messages.first().map(|m| &m.role),
            Some(maki_providers::Role::Assistant)
        )
    {
        let mut drop = 1;
        if matches!(
            messages.get(1).map(|m| &m.role),
            Some(maki_providers::Role::User)
        ) {
            drop = 2;
        }
        messages.drain(..drop);
    }
}

pub(super) fn auto_compact_enabled() -> bool {
    env::var("MAKI_DISABLE_AUTOCOMPACT")
        .map(|v| v != "1" && v != "true")
        .unwrap_or(true)
}

#[cfg(test)]
mod tests;
