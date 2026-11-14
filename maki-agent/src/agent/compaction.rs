use std::collections::HashMap;
use std::env;
use std::time::{Duration, Instant};

use maki_config::AgentConfig;
use maki_providers::retry::RetryPolicy;
use maki_providers::{
    ContentBlock, ContextGauge, IMAGE_PLACEHOLDER, Message, Model, RequestOptions, Role,
    StreamResponse,
};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::frame::{RunContext, fit_frame};
use super::history::{History, remove_orphaned_tool_results, stamp_new, strip_thinking};
use super::hook::{AgentHooks, AgentSlot};
use super::streaming::{StreamError, StreamRequest, min_output, stream_with_retry};
use crate::mcp::McpSession;
use crate::prompt::COMPACTION_USER;
use crate::tools::hook::Verdict;
use crate::tools::truncate_bytes;
use crate::{
    AgentError, AgentEvent, DoneReason, EventSender, RunLedger, SteerKind, TurnCompleteEvent,
};

const CONTINUE_AFTER_COMPACT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed. If the summary contains a todo list, restore it with todo_write and keep it updated. If you learned important project context during this session, consider saving it to memory before it's lost.";
const TOOL_RESULT_PLACEHOLDER: &str = "[tool result]";
/// How much of the newest tool output survives a compaction verbatim. This
/// used to be a count, which kept three huge results and threw away thirty
/// cheap ones, so one giant MCP dump could sit in the protected tail and blow
/// the window on every retry.
const RECENT_TOOL_RESULT_BUDGET: usize = 64 * 1024;
/// A summary runs to a few thousand tokens. Giving compaction the full turn
/// budget would reserve tens of thousands it cannot use, on the one request
/// already sent under the most context pressure a session ever sees.
const SUMMARY_OUTPUT_BUDGET: u32 = 16_384;
/// Ceiling on everything [`reserved`] holds back, as a share of the window.
///
/// The floor under a reservation is a fixed token count and the buffer can be
/// written as one, so on a small window either can reach the whole context. A
/// reservation that large leaves no usable window at all: every turn reads as
/// an overflow, and compaction cannot get under a threshold of zero, so a
/// llama.cpp server started with `n_ctx 4096` summarized itself before every
/// single turn.
const MAX_RESERVED_PERCENT: u32 = 50;
/// A layer only needs a taste of each result to judge it. Shipping a 40 KB
/// build log to Lua for every result of a long session would cost more than
/// the layer saves. `bytes` still tells the full size.
const PREPARE_TEXT_MAX: usize = 4 * 1024;
/// Overflow pruning and the empty summary retry share this budget.
const MAX_SUMMARY_ATTEMPTS: u32 = 3;

pub(super) const FIELD_SKIP: &str = "skip";
const FIELD_INSTRUCTIONS: &str = "instructions";
pub(super) const FIELD_CONTINUE: &str = "continue";
pub(super) const FIELD_DEFER: &str = "defer";
const FIELD_COLLAPSE: &str = "collapse";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CompactReason {
    Auto,
    /// The provider already refused the prompt as too long. Compaction is the
    /// only way out, so a layer cannot skip this one.
    Overflow,
    Manual,
}

impl CompactReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Overflow => "overflow",
            Self::Manual => "manual",
        }
    }
}

/// Both strings go on top of the configured ones and never replace them, so a
/// plugin cannot quietly drop what the user put in config.
#[derive(Default)]
pub(super) struct CompactSteer {
    /// Already holds the `/compact` request's own words, with the layer's
    /// after them.
    pub instructions: Option<String>,
    pub continue_text: Option<String>,
}

fn join_lines<'a>(parts: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    let joined = parts
        .into_iter()
        .filter_map(normalize)
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

/// `agent.stop` and `agent.compact.before` both resume the run with it.
pub(super) fn continue_text(value: &Value) -> Option<&str> {
    normalize(value.get(FIELD_CONTINUE).and_then(Value::as_str))
}

/// What a layer settled on for a compaction.
pub(super) enum CompactionPlan {
    /// The compaction is skipped. The gauge stays where it was, so an
    /// automatic one simply asks again before the next turn.
    Skipped,
    /// The layer bought the transcript one turn: the nudge is the model's
    /// next message and the ask after that turn is granted. One defer per
    /// compaction, automatic only, so a layer cannot starve the gauge.
    Deferred(String),
    Proceed(CompactSteer),
}

pub(super) async fn steer_compaction(
    hooks: &AgentHooks<'_>,
    config: &AgentConfig,
    reason: CompactReason,
    request: Option<&str>,
    can_defer: bool,
) -> CompactionPlan {
    let verdict = hooks
        .fire(AgentSlot::CompactBefore, || {
            json!({
                "reason": reason.as_str(),
                "context_size": hooks.context_size,
                "usable": usable(hooks.model, config),
                "request_instructions": request,
            })
        })
        .await;
    let (skip, added, continue_with, defer) = match &verdict {
        Verdict::Replaced(value) => (
            value
                .get(FIELD_SKIP)
                .and_then(Value::as_bool)
                .unwrap_or(false),
            value.get(FIELD_INSTRUCTIONS).and_then(Value::as_str),
            continue_text(value),
            normalize(value.get(FIELD_DEFER).and_then(Value::as_str)),
        ),
        Verdict::Denied(why) => {
            info!(reason = %why, "agent.compact.before asked to skip");
            (true, None, None, None)
        }
        Verdict::Unchanged | Verdict::Ask { .. } => (false, None, None, None),
    };
    // A defer is one turn, so it works the same in a task child as in main.
    if can_defer && let Some(nudge) = defer {
        info!("agent.compact.before deferred the compaction");
        return CompactionPlan::Deferred(nudge.to_owned());
    }
    if skip && reason != CompactReason::Overflow {
        info!(
            reason = reason.as_str(),
            "agent.compact.before skipped the compaction"
        );
        return CompactionPlan::Skipped;
    }
    CompactionPlan::Proceed(CompactSteer {
        instructions: join_lines([request, added]),
        continue_text: continue_with.map(str::to_owned),
    })
}

fn percent_of(tokens: u32, percent: u32) -> u32 {
    (u64::from(tokens) * u64::from(percent) / 100) as u32
}

fn normalize(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

/// Config instructions steer every compaction, `request` only the one the user
/// asked for with `/compact <guidance>`, so both are kept and neither wins.
fn summary_prompt(config: &AgentConfig, request: Option<&str>) -> String {
    let extras = [
        normalize(config.compaction_instructions.as_deref()),
        normalize(request),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n");
    if extras.is_empty() {
        return COMPACTION_USER.to_string();
    }
    format!("{COMPACTION_USER}\n\nAdditional instructions:\n{extras}")
}

pub(super) fn continue_message(added: Option<&str>) -> String {
    match added {
        Some(extra) => format!("{CONTINUE_AFTER_COMPACT}\n\n{extra}"),
        None => CONTINUE_AFTER_COMPACT.to_string(),
    }
}

pub(super) struct Compacted {
    pub summary: String,
    pub duration: Duration,
    /// How many unanswered messages follow the summary. Context updates are
    /// never among them, see [`History::restart`].
    pub carried: usize,
}

/// Replaces `history` with a summary of itself, retrying on overflow by
/// pruning what it sends, and once on an empty summary.
///
/// The last `carry_len` messages are held out of the summary and re-appended
/// after it, so input no turn has answered yet survives verbatim without ever
/// leaving `history`. Anything less and the mirror would publish a transcript
/// missing that input for the length of the summary request.
#[allow(clippy::too_many_arguments)]
pub(super) async fn compact_history(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    hooks: &AgentHooks<'_>,
    config: &AgentConfig,
    instructions: Option<&str>,
    carry_len: usize,
    retry: RetryPolicy,
    ledger: &RunLedger,
) -> Result<Compacted, AgentError> {
    let compact_start = Instant::now();
    event_tx.send(AgentEvent::CompactionStarted)?;
    let summarized = history.len().saturating_sub(carry_len);
    let mut compaction_history: Vec<Message> = history.as_slice()[..summarized].to_vec();
    remove_orphaned_tool_results(&mut compaction_history);
    strip_images(&mut compaction_history);
    strip_thinking(&mut compaction_history);
    prepare_collapse(hooks, &mut compaction_history, RECENT_TOOL_RESULT_BUDGET).await;
    compaction_history.push(Message::user(summary_prompt(config, instructions)));

    let empty_tools = serde_json::json!([]);
    let mut retried_empty = false;
    let mut attempt = 0;

    loop {
        let can_retry = attempt + 1 < MAX_SUMMARY_ATTEMPTS;
        match stream_with_retry(
            StreamRequest {
                provider,
                model,
                messages: &compaction_history,
                system: crate::prompt::COMPACTION_SYSTEM,
                tools: &empty_tools,
                opts: RequestOptions::default(),
                output_budget: SUMMARY_OUTPUT_BUDGET,
                session_id: hooks.session_id,
                retry,
            },
            // A stripped, collapsed rewrite of the transcript, far smaller than
            // it. Sizing this request as the session would trim the
            // summariser's budget by the weight of the messages it is deleting,
            // and its measurement describes a prompt the session never had.
            None,
            event_tx,
            hooks.cancel,
        )
        .await
        {
            Ok(response) => {
                // The summary request never asks for fast mode, so it costs the
                // standard rate. It is banked before the text is checked,
                // because an unusable summary was still paid for.
                ledger.add(
                    response.usage,
                    model.billed_cost(&response.usage, false),
                    model.list_cost(&response.usage, false),
                );
                let stop_reason = response.stop_reason;
                match finish_compact(
                    response, history, summarized, event_tx, model, hooks, config,
                ) {
                    // Just once. This is the biggest request a session sends,
                    // and a model that comes back empty twice, say because its
                    // thinking eats the whole budget, will keep doing it.
                    Err(AgentError::EmptySummary) if can_retry && !retried_empty => {
                        retried_empty = true;
                        warn!(
                            attempt,
                            model = %model.id,
                            ?stop_reason,
                            "compaction returned no summary text, retrying"
                        );
                    }
                    result => {
                        let (summary, carried) = result?;
                        let duration = compact_start.elapsed();
                        info!(
                            model = %model.id,
                            duration_ms = duration.as_millis() as u64,
                            attempt,
                            "compaction completed"
                        );
                        return Ok(Compacted {
                            summary,
                            duration,
                            carried,
                        });
                    }
                }
            }
            Err(StreamError::Other(e)) if can_retry && e.is_context_overflow() => {
                // Truncation eats from the front, so it can never shrink an
                // oversized result sitting in the protected tail. Collapse that
                // first, and once there is nothing left to collapse, drop the
                // oldest round rather than resend the same request.
                if !collapse_tool_results(&mut compaction_history, 0) {
                    truncate_oldest_round(&mut compaction_history);
                }
            }
            Err(e) => return Err(e.into()),
        }
        attempt += 1;
    }
}

/// Rebuild the transcript after a compaction: a summary pair, then the tail that
/// was not summarized. Returns the new list and the length of the summary pair,
/// so the caller can report how many messages carried over.
///
/// The summary pair carries the first kept message's stamp, not "now". The pair
/// is followed by the carried tail, whose stamps predate the summary call that
/// just returned; stamping the pair now would run the transcript's clock backwards
/// at the join, and a timing reader would see a negative gap. The compaction
/// call's own wall time then falls to the first carried message instead.
fn rebuild_history(
    summary: Message,
    history: &[Message],
    summarized: usize,
    post_compaction_instructions: Option<&str>,
) -> (Vec<Message>, usize) {
    let carried_at = history.get(summarized).and_then(|m| m.created_at);
    let stamp = |msg: Message| {
        stamp_new(Message {
            created_at: carried_at,
            ..msg
        })
    };
    let mut new_history = vec![
        stamp(Message::user("What did we do so far?".into())),
        stamp(summary),
    ];
    // Part of the summary block and not the carried input, so the next
    // compaction summarizes this copy away instead of carrying a duplicate.
    new_history.extend(
        normalize(post_compaction_instructions)
            .map(|text| stamp(Message::synthetic(text.to_owned()))),
    );
    let summary_len = new_history.len();
    new_history.extend_from_slice(&history[summarized..]);
    (new_history, summary_len)
}

fn finish_compact(
    response: StreamResponse,
    history: &mut History,
    summarized: usize,
    event_tx: &EventSender,
    model: &Model,
    hooks: &AgentHooks<'_>,
    config: &AgentConfig,
) -> Result<(String, usize), AgentError> {
    let summary = response.message.first_text_content().map(str::to_owned);
    // An unusable summary leaves the session as it was, so it keeps its size.
    let context_size = if summary.is_some() {
        response.usage.output.min(u32::MAX as u64) as u32
    } else {
        hooks.context_size
    };
    let _ = event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        cost: model.billed_cost(&response.usage, false),
        subsidised_list_cost: model.subsidised_list_cost(&response.usage, false),
        context_size: Some(context_size),
        context_window: hooks.model.context_window,
    })));

    // Swapping the history for a summary the model never wrote would throw the
    // session away for nothing.
    let Some(summary) = summary else {
        return Err(AgentError::EmptySummary);
    };

    let (new_history, summary_len) = rebuild_history(
        response.message,
        history.as_slice(),
        summarized,
        config.post_compaction_instructions.as_deref(),
    );
    history.restart(new_history);

    Ok((summary, history.len() - summary_len))
}

/// `next` is what the session would start with now. The summary goes out under
/// a fresh frame built from it, for the session's own model in `hooks`, so the
/// gauge measures the prompt the next request really sends and the frame never
/// goes missing between runs. `model` is the one writing the summary.
///
/// A retry in here can honour a server `Retry-After` that parks the request for
/// an hour, so esc has to reach it. The cancel comes back as
/// `Ok(DoneReason::Cancelled)`, like [`Agent::run`](super::Agent::run) does, to
/// leave the error path for real failures.
#[allow(clippy::too_many_arguments)]
pub async fn compact(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    gauge: &mut ContextGauge,
    next: RunContext,
    mcp: Option<&McpSession>,
    event_tx: &EventSender,
    hooks: &AgentHooks<'_>,
    config: &AgentConfig,
    instructions: Option<&str>,
    retry: RetryPolicy,
) -> Result<DoneReason, AgentError> {
    let size_before = gauge.size();
    let ledger = RunLedger::default();
    let finished = |context_size| {
        let totals = ledger.totals();
        AgentEvent::Done {
            usage: totals.usage,
            cost: totals.cost,
            list_cost: totals.list_cost,
            context_size,
            context_window: hooks.model.context_window,
            num_turns: 1,
            // `Compact` and not `EndTurn`, so a goal loop reading this sees
            // housekeeping and does not treat it as a turn boundary.
            reason: DoneReason::Compact,
        }
    };
    let CompactionPlan::Proceed(steer) =
        steer_compaction(hooks, config, CompactReason::Manual, instructions, false).await
    else {
        event_tx.send(finished(size_before))?;
        return Ok(DoneReason::Compact);
    };
    let Compacted {
        summary, duration, ..
    } = match compact_history(
        provider,
        model,
        history,
        event_tx,
        hooks,
        config,
        steer.instructions.as_deref(),
        0,
        retry,
        &ledger,
    )
    .await
    {
        Ok(done) => done,
        // `finish_compact` is the only writer in here, so a cancel leaves the
        // transcript and the gauge untouched and the session carries on.
        Err(AgentError::Cancelled) => return Ok(DoneReason::Cancelled),
        Err(e) => return Err(e),
    };
    if let Some(text) = steer.continue_text {
        event_tx.send(AgentEvent::Steered {
            kind: SteerKind::CompactContinued,
            text: text.clone(),
        })?;
        history.push(Message::synthetic(text));
    }
    fit_frame(history, next, mcp, hooks.model);
    if let Some((system, tools)) = history.request_prefix() {
        gauge.reset(history.as_slice(), system, tools);
    }

    // The summariser read a subset of the session's prompt, so its own count is
    // a floor on the size before, and the only number a gauge that has not seen
    // a turn yet can offer.
    let context_size_before =
        size_before.max(ledger.totals().usage.total_input().min(u32::MAX as u64) as u32);
    let context_size_after = gauge.size();
    event_tx.send(AgentEvent::CompactionDone {
        context_size_before,
        context_size_after,
        context_window: hooks.model.context_window,
        summary,
        duration_ms: duration.as_millis() as u64,
    })?;
    event_tx.send(finished(context_size_after))?;

    Ok(DoneReason::Compact)
}

/// Context held back from the transcript.
///
/// Never below [`min_output`], the floor a request may ask for, so under this
/// threshold the window still houses the prompt and an answer and a server
/// checking `prompt + max_tokens <= context_window` has nothing to object to.
/// That floor tracks the model's own cap, or a small-window model would compact
/// itself over output tokens it could never emit.
///
/// Reserving a whole [`AgentConfig::max_turn_output`] would guarantee more and
/// cost more: on a small window that is half the context, and compacting that
/// early hurts worse than the odd turn whose output budget gets trimmed.
///
/// Whatever the floor and the buffer work out to, [`MAX_RESERVED_PERCENT`] has
/// the last word, because a reservation that eats the window leaves compaction
/// nothing to compact into.
pub(super) fn reserved(model: &Model, config: &AgentConfig) -> u32 {
    config
        .compaction_buffer
        .resolve(model.context_window)
        .max(min_output(model))
        .min(percent_of(model.context_window, MAX_RESERVED_PERCENT))
}

/// What [`reserved`] leaves the transcript. Always a real number of tokens, so
/// `>=` against it is a threshold a session can sit below.
pub(super) fn usable(model: &Model, config: &AgentConfig) -> u32 {
    model.context_window - reserved(model, config)
}

pub(super) fn is_overflow(context_tokens: u64, model: &Model, config: &AgentConfig) -> bool {
    context_tokens >= u64::from(usable(model, config))
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

/// Where every tool result sits, oldest first, as `(message, block)`.
fn tool_result_positions(messages: &[Message]) -> Vec<(usize, usize)> {
    messages
        .iter()
        .enumerate()
        .flat_map(|(m, msg)| {
            msg.content
                .iter()
                .enumerate()
                .filter(|(_, block)| matches!(block, ContentBlock::ToolResult { .. }))
                .map(move |(b, _)| (m, b))
        })
        .collect()
}

fn tool_result(messages: &[Message], (m, b): (usize, usize)) -> (&str, &str) {
    match &messages[m].content[b] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => (tool_use_id, content),
        _ => ("", ""),
    }
}

/// Walks newest first and marks every result that does not fit in what is left
/// of `budget`. One oversized result gets marked on its own and leaves the
/// budget to the older ones. That is the whole point: charging it would spend
/// the tail on a block that is no longer there.
fn default_collapse(
    messages: &[Message],
    positions: &[(usize, usize)],
    mut budget: usize,
) -> Vec<bool> {
    let mut collapse = vec![false; positions.len()];
    for (i, &pos) in positions.iter().enumerate().rev() {
        match budget.checked_sub(tool_result(messages, pos).1.len()) {
            Some(rest) => budget = rest,
            None => collapse[i] = true,
        }
    }
    collapse
}

/// Returns whether anything actually shrank, so a caller retrying on overflow
/// can tell progress from a no-op.
fn apply_collapse(
    messages: &mut [Message],
    positions: &[(usize, usize)],
    collapse: &[bool],
) -> bool {
    let mut collapsed = false;
    for (&(m, b), _) in positions.iter().zip(collapse).filter(|(_, c)| **c) {
        if let ContentBlock::ToolResult { content, .. } = &mut messages[m].content[b] {
            collapsed |= content != TOOL_RESULT_PLACEHOLDER;
            *content = TOOL_RESULT_PLACEHOLDER.into();
        }
    }
    collapsed
}

fn collapse_tool_results(messages: &mut [Message], budget: usize) -> bool {
    let positions = tool_result_positions(messages);
    let collapse = default_collapse(messages, &positions, budget);
    apply_collapse(messages, &positions, &collapse)
}

/// The host's own pick rides along in `collapse`, so a layer that only wants
/// to spare one result edits that list and hands it back, instead of redoing
/// the budget math. Indices start at 1 because a Lua list does.
fn prepare_value(
    messages: &[Message],
    positions: &[(usize, usize)],
    collapse: &[bool],
    budget: usize,
) -> Value {
    let calls: HashMap<&str, (&str, &Value)> = messages
        .iter()
        .flat_map(Message::tool_uses)
        .map(|(id, name, input)| (id, (name, input)))
        .collect();
    let results: Vec<Value> = positions
        .iter()
        .enumerate()
        .map(|(i, &pos)| {
            let (tool_use_id, text) = tool_result(messages, pos);
            let (tool, input) = calls
                .get(tool_use_id)
                .map_or((None, None), |(name, input)| (Some(*name), Some(*input)));
            json!({
                "index": i + 1,
                "tool": tool,
                "input": input,
                "bytes": text.len(),
                "text": truncate_bytes(text, PREPARE_TEXT_MAX),
            })
        })
        .collect();
    let picked: Vec<usize> = collapse
        .iter()
        .enumerate()
        .filter(|(_, c)| **c)
        .map(|(i, _)| i + 1)
        .collect();
    json!({ "results": results, "budget": budget, FIELD_COLLAPSE: picked })
}

/// The layer only picks. The host still makes the edit, so no layer can
/// orphan a tool call or lose the user's message along the way.
async fn prepare_collapse(hooks: &AgentHooks<'_>, messages: &mut [Message], budget: usize) {
    let positions = tool_result_positions(messages);
    if positions.is_empty() {
        return;
    }
    let mut collapse = default_collapse(messages, &positions, budget);
    let verdict = hooks
        .fire(AgentSlot::CompactPrepare, || {
            prepare_value(messages, &positions, &collapse, budget)
        })
        .await;
    if let Verdict::Replaced(value) = verdict
        && let Some(picked) = value.get(FIELD_COLLAPSE).and_then(Value::as_array)
    {
        collapse.fill(false);
        let indices = picked.iter().filter_map(Value::as_u64);
        for i in indices.filter_map(|index| (index as usize).checked_sub(1)) {
            if let Some(slot) = collapse.get_mut(i) {
                *slot = true;
            }
        }
    }
    apply_collapse(messages, &positions, &collapse);
}

fn truncate_oldest_round(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }

    let removed_user = matches!(messages.remove(0).role, Role::User);
    if removed_user
        && messages.len() > 1
        && matches!(
            messages.first().map(|message| &message.role),
            Some(Role::Assistant)
        )
    {
        messages.remove(0);
    }
    remove_orphaned_tool_results(messages);

    while messages.len() > 1
        && matches!(
            messages.first().map(|message| &message.role),
            Some(Role::Assistant)
        )
    {
        messages.remove(0);
        remove_orphaned_tool_results(messages);
    }
}

pub(super) fn auto_compact_enabled() -> bool {
    env::var("MAKI_DISABLE_AUTOCOMPACT")
        .map(|v| v != "1" && v != "true")
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::sync::Mutex;

    use maki_providers::provider::{BoxFuture, Provider};
    use maki_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use maki_storage::id::SessionRef;
    use serde_json::Value;
    use test_case::test_case;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::AgentConfig;
    use crate::agent::hook::testing::script;
    use crate::cancel::CancelToken;
    use crate::tools::{RequestTools, ToolRegistry};
    use maki_config::CompactionBuffer;

    #[test]
    fn rebuild_history_keeps_timestamps_monotonic_at_the_join() {
        let history = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "summarized".into(),
                }],
                created_at: Some(100),
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "first kept".into(),
                }],
                created_at: Some(1_000),
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "kept".into(),
                }],
                created_at: Some(2_000),
                ..Default::default()
            },
        ];
        let summary = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "the summary".into(),
            }],
            ..Default::default()
        };

        let (rebuilt, summary_len) = rebuild_history(summary, &history, 1, None);
        assert_eq!(
            summary_len, 2,
            "a prompt and its summary head the transcript"
        );
        assert_eq!(rebuilt.len(), 2 + 2, "pair + the carried tail");
        // The pair takes the first kept message's stamp (1_000), never "now": the
        // summary call started after the tail was written, so "now" would postdate
        // it and run the transcript clock backwards at the join.
        assert_eq!(rebuilt[0].created_at, Some(1_000));
        assert_eq!(rebuilt[1].created_at, Some(1_000));
        let mut prev = 0;
        for msg in &rebuilt {
            let at = msg.created_at.expect("every rebuilt message is stamped");
            assert!(at >= prev, "timestamps must not go backwards: {rebuilt:?}");
            prev = at;
        }
    }

    const CONFIG_EXTRA: &str = "Record anything that belongs in plan.md";
    const REQUEST_EXTRA: &str = "Keep the failing test names";
    const POST: &str = "Re-read plan.md and agent.md";
    const OVERFLOW_MESSAGE: &str = "prompt is too long";
    const OVERFLOW_STATUS: u16 = 413;
    /// Differs from the session window, so a report that mixes them up shows.
    const SUMMARY_CONTEXT_WINDOW: u32 = 128_000;
    /// Shorter than [`NEW_RESULT`], so the budget-burn case below is the only
    /// reason it collapses.
    const OLD_RESULT: &str = "old";
    const NEW_RESULT: &str = "new result";
    const KEPT_TEXT: &str = "keep me";
    /// These tests assert on the transcript and on gauge sizes relative to each
    /// other, so the baseline would only add noise.
    const NO_SYSTEM: &str = "";

    fn bare_context(model: &Model) -> RunContext {
        RunContext::fixed(
            NO_SYSTEM.into(),
            RequestTools::assembled(Value::Array(Vec::new()), &AgentConfig::default(), model),
        )
    }

    struct MockProvider {
        responses: Mutex<Vec<Result<StreamResponse, AgentError>>>,
        requests: Mutex<Vec<Vec<Message>>>,
        sessions: Mutex<Vec<Option<String>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<Result<StreamResponse, AgentError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
                sessions: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a UnboundedSender<ProviderEvent>,
            _: RequestOptions,
            session_id: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(messages.to_vec());
                self.sessions
                    .lock()
                    .unwrap()
                    .push(session_id.map(|s| s.as_str().to_string()));
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                responses.remove(0)
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn default_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn small_context_model(context_window: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model
    }

    fn overflow_error() -> AgentError {
        AgentError::api(OVERFLOW_STATUS, OVERFLOW_MESSAGE)
    }

    fn text_response(stop_reason: StopReason) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(stop_reason),
            ..Default::default()
        }
    }

    fn test_hooks<'a>(
        registry: &'a ToolRegistry,
        session_id: Option<&'a SessionRef>,
        model: &'a Model,
        cancel: &'a CancelToken,
    ) -> AgentHooks<'a> {
        AgentHooks {
            registry,
            session_id,
            task_id: None,
            model,
            cancel,
            context_size: 0,
        }
    }

    async fn summarize(
        provider: &MockProvider,
        history: &mut History,
        gauge: &mut ContextGauge,
        config: &AgentConfig,
        instructions: Option<&str>,
        cancel: &CancelToken,
    ) -> Result<DoneReason, AgentError> {
        let (raw_tx, _rx) = unbounded_channel();
        let registry = ToolRegistry::new();
        let model = default_model();
        compact(
            provider,
            &model,
            history,
            gauge,
            bare_context(&model),
            None,
            &EventSender::new(raw_tx, 0),
            &test_hooks(&registry, None, &model, cancel),
            config,
            instructions,
            RetryPolicy::default(),
        )
        .await
    }

    async fn summarize_history(
        provider: &MockProvider,
        history: &mut History,
        carry_len: usize,
        session_id: Option<&SessionRef>,
    ) {
        let (raw_tx, _rx) = unbounded_channel();
        let registry = ToolRegistry::new();
        let model = default_model();
        let cancel = CancelToken::none();
        compact_history(
            provider,
            &model,
            history,
            &EventSender::new(raw_tx, 0),
            &test_hooks(&registry, session_id, &model, &cancel),
            &AgentConfig::default(),
            None,
            carry_len,
            RetryPolicy::default(),
            &RunLedger::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn compact_replaces_history_with_summary() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![
            Message::user("first".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
        ]);

        summarize(
            &provider,
            &mut history,
            &mut ContextGauge::default(),
            &AgentConfig::default(),
            None,
            &CancelToken::none(),
        )
        .await
        .unwrap();

        let msgs = history.as_slice();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));
        assert!(
            history.request_prefix().is_some(),
            "a standalone compaction leaves the session a frame, or `/btw` and the gauge have no prompt to read"
        );
    }

    const SUMMARISED_PROMPT: u32 = 150_000;
    const SUMMARISED_INPUT: u64 = 150_000;

    /// The measurement the gauge holds is of the transcript compaction just
    /// deleted, so left in place it sizes every later turn against a session
    /// that no longer exists.
    #[tokio::test]
    async fn compact_leaves_the_gauge_describing_the_summary() {
        let provider = MockProvider::new(vec![Ok(StreamResponse {
            usage: TokenUsage {
                input: SUMMARISED_INPUT,
                ..Default::default()
            },
            ..text_response(StopReason::EndTurn)
        })]);
        let mut history = History::new(vec![Message::user("first".into())]);
        let mut gauge = ContextGauge::restored(SUMMARISED_PROMPT);

        summarize(
            &provider,
            &mut history,
            &mut gauge,
            &AgentConfig::default(),
            None,
            &CancelToken::none(),
        )
        .await
        .unwrap();

        assert!(
            gauge.size() < SUMMARISED_PROMPT,
            "the gauge still sizes the session by the transcript it summarized away"
        );
    }

    #[test_case(vec![] ; "no_content")]
    #[test_case(vec![ContentBlock::Text { text: " \n".into() }] ; "blank_text")]
    #[test_case(vec![ContentBlock::Thinking { thinking: KEPT_TEXT.into(), signature: None, reasoning_id: None }] ; "thinking_only")]
    #[tokio::test]
    async fn a_failed_compaction_leaves_the_context_alone(content: Vec<ContentBlock>) {
        let empty = || {
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: content.clone(),
                    ..Default::default()
                },
                usage: TokenUsage {
                    input: SUMMARISED_INPUT / 2,
                    output: SUMMARISED_INPUT / 10,
                    ..Default::default()
                },
                stop_reason: Some(StopReason::EndTurn),
                ..Default::default()
            })
        };
        let provider = MockProvider::new(vec![empty(), empty()]);
        let mut history = History::new(vec![Message::user(KEPT_TEXT.into())]);
        let mut gauge = ContextGauge::restored(SUMMARISED_PROMPT);
        let (raw_tx, mut rx) = unbounded_channel();
        let registry = ToolRegistry::new();
        let model = default_model();
        let cancel = CancelToken::none();
        let mut hooks = test_hooks(&registry, None, &model, &cancel);
        hooks.context_size = gauge.size();

        let result = compact(
            &provider,
            &small_context_model(SUMMARY_CONTEXT_WINDOW),
            &mut history,
            &mut gauge,
            bare_context(&model),
            None,
            &EventSender::new(raw_tx, 0),
            &hooks,
            &AgentConfig::default(),
            None,
            RetryPolicy::default(),
        )
        .await;

        assert!(matches!(result, Err(AgentError::EmptySummary)));
        assert_eq!(gauge.size(), SUMMARISED_PROMPT);
        assert_eq!(history.len(), 1);
        assert_eq!(history.as_slice()[0].user_text(), Some(KEPT_TEXT));
        let events: Vec<_> = iter::from_fn(|| rx.try_recv().ok())
            .map(|e| e.event)
            .collect();
        let [
            AgentEvent::CompactionStarted,
            AgentEvent::TurnComplete(first),
            AgentEvent::TurnComplete(retry),
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        for turn in [first, retry] {
            assert_eq!(turn.context_size, Some(SUMMARISED_PROMPT));
            assert_eq!(turn.context_window, model.context_window);
        }
    }

    const ATTEMPT_INPUT: u64 = 1_000;

    enum Reply {
        Overflow,
        Empty,
        Summary,
    }

    fn scripted(reply: &Reply) -> Result<StreamResponse, AgentError> {
        let usage = TokenUsage {
            input: ATTEMPT_INPUT,
            ..Default::default()
        };
        match reply {
            Reply::Overflow => Err(overflow_error()),
            Reply::Empty => Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    ..Default::default()
                },
                usage,
                stop_reason: Some(StopReason::MaxTokens),
                ..Default::default()
            }),
            Reply::Summary => Ok(StreamResponse {
                usage,
                ..text_response(StopReason::EndTurn)
            }),
        }
    }

    /// Every attempt that came back is a paid request, so each reports its own
    /// turn and the `Done` usage sums them all.
    #[test_case(&[Reply::Empty, Reply::Summary], true ; "empty_then_summary")]
    #[test_case(&[Reply::Overflow, Reply::Empty, Reply::Summary], true ; "overflow_then_empty_then_summary")]
    #[test_case(&[Reply::Overflow, Reply::Overflow, Reply::Empty], false ; "no_retry_past_the_attempt_budget")]
    #[tokio::test]
    async fn empty_summary_retry(replies: &[Reply], summarised: bool) {
        let provider = MockProvider::new(replies.iter().map(scripted).collect());
        let mut history = History::new(vec![
            Message::user(KEPT_TEXT.into()),
            Message::user("second".into()),
        ]);
        let (raw_tx, mut rx) = unbounded_channel();
        let registry = ToolRegistry::new();
        let model = default_model();
        let cancel = CancelToken::none();

        let result = compact(
            &provider,
            &model,
            &mut history,
            &mut ContextGauge::default(),
            bare_context(&model),
            None,
            &EventSender::new(raw_tx, 0),
            &test_hooks(&registry, None, &model, &cancel),
            &AgentConfig::default(),
            None,
            RetryPolicy::default(),
        )
        .await;

        assert_eq!(provider.requests.lock().unwrap().len(), replies.len());
        let answered = replies
            .iter()
            .filter(|reply| !matches!(reply, Reply::Overflow))
            .count();
        let events: Vec<_> = iter::from_fn(|| rx.try_recv().ok())
            .map(|e| e.event)
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::CompactionStarted))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::CompactionDone { .. }))
                .count(),
            usize::from(summarised)
        );
        let turns = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnComplete(_)))
            .count();
        assert_eq!(turns, answered);
        let kept = history
            .as_slice()
            .iter()
            .any(|message| message.user_text() == Some(KEPT_TEXT));
        assert_eq!(kept, !summarised);
        if summarised {
            assert_eq!(result.unwrap(), DoneReason::Compact);
            let Some(AgentEvent::Done { usage, .. }) = events.last() else {
                panic!("{events:?}");
            };
            assert_eq!(usage.input, ATTEMPT_INPUT * answered as u64);
        } else {
            assert!(matches!(result, Err(AgentError::EmptySummary)));
        }
    }

    #[tokio::test]
    async fn compact_reports_a_cancel_as_an_ending_not_a_failure() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![Message::user(KEPT_TEXT.into())]);
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();

        let reason = summarize(
            &provider,
            &mut history,
            &mut ContextGauge::default(),
            &AgentConfig::default(),
            None,
            &cancel,
        )
        .await
        .expect("a cancel is an ending, not a failure");

        assert_eq!(reason, DoneReason::Cancelled);
        assert!(
            provider.requests.lock().unwrap().is_empty(),
            "the cancel should have landed before the request went out"
        );
        assert_eq!(
            history.as_slice()[0].user_text(),
            Some(KEPT_TEXT),
            "a cancelled compaction must leave the transcript alone"
        );
    }

    /// `summary_prompt_merges_instructions` covers the merge, this one the
    /// wiring around it.
    #[tokio::test]
    async fn compact_sends_instructions_and_appends_post() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![Message::user("work".into())]);
        let config = AgentConfig {
            compaction_instructions: Some(CONFIG_EXTRA.into()),
            post_compaction_instructions: Some(POST.into()),
            ..Default::default()
        };

        summarize(
            &provider,
            &mut history,
            &mut ContextGauge::default(),
            &config,
            Some(REQUEST_EXTRA),
            &CancelToken::none(),
        )
        .await
        .unwrap();

        let requests = provider.requests.lock().unwrap();
        assert!(matches!(
            &requests[0].last().unwrap().content[0],
            ContentBlock::Text { text }
                if text.contains(CONFIG_EXTRA) && text.contains(REQUEST_EXTRA)
        ));
        assert!(matches!(
            &history.as_slice().last().unwrap().content[0],
            ContentBlock::Text { text } if text == POST
        ));
    }

    const LAYER_EXTRA: &str = "Name every file you touched";
    const SUMMARY: &str = "We fixed the flaky test in run.rs";

    fn before_answers(value: Value) -> impl Fn(AgentSlot, &Value) -> Verdict + Send + Sync {
        move |slot, _| match slot {
            AgentSlot::CompactBefore => Verdict::Replaced(value.clone()),
            _ => Verdict::Unchanged,
        }
    }

    /// Runs the `/compact` path with a gauge at [`SUMMARISED_PROMPT`] and
    /// hands back every event it sent.
    async fn summarize_steered(
        provider: &MockProvider,
        history: &mut History,
        instructions: Option<&str>,
        answer: impl Fn(AgentSlot, &Value) -> Verdict + Send + Sync + 'static,
    ) -> (Result<DoneReason, AgentError>, Vec<AgentEvent>) {
        let (raw_tx, mut rx) = unbounded_channel();
        let registry = ToolRegistry::new();
        script(&registry, answer);
        let model = default_model();
        let cancel = CancelToken::none();
        let result = compact(
            provider,
            &small_context_model(SUMMARY_CONTEXT_WINDOW),
            history,
            &mut ContextGauge::restored(SUMMARISED_PROMPT),
            bare_context(&model),
            None,
            &EventSender::new(raw_tx, 0),
            &test_hooks(&registry, None, &model, &cancel),
            &AgentConfig::default(),
            instructions,
            RetryPolicy::default(),
        )
        .await;
        (
            result,
            iter::from_fn(|| rx.try_recv().ok())
                .map(|e| e.event)
                .collect(),
        )
    }

    /// The mock holds no responses, so any request would panic.
    #[tokio::test]
    async fn manual_compact_skipped_by_layer_ends_without_a_request() {
        let provider = MockProvider::new(Vec::new());
        let mut history = History::new(vec![Message::user(KEPT_TEXT.into())]);

        let (result, events) = summarize_steered(
            &provider,
            &mut history,
            None,
            before_answers(json!({ FIELD_SKIP: true })),
        )
        .await;

        assert_eq!(result.unwrap(), DoneReason::Compact);
        assert_eq!(history.len(), 1);
        assert_eq!(history.as_slice()[0].user_text(), Some(KEPT_TEXT));
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::CompactionStarted | AgentEvent::CompactionDone { .. }
        )));
        let done: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Done {
                    usage,
                    context_size,
                    context_window,
                    reason,
                    ..
                } => Some((*usage, *context_size, *context_window, *reason)),
                _ => None,
            })
            .collect();
        assert_eq!(
            done,
            [(
                TokenUsage::default(),
                SUMMARISED_PROMPT,
                default_model().context_window,
                DoneReason::Compact
            )]
        );
    }

    #[tokio::test]
    async fn layer_instructions_follow_the_requests_own() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![Message::user("work".into())]);

        let (result, _) = summarize_steered(
            &provider,
            &mut history,
            Some(REQUEST_EXTRA),
            before_answers(json!({ FIELD_INSTRUCTIONS: LAYER_EXTRA })),
        )
        .await;
        result.unwrap();

        let requests = provider.requests.lock().unwrap();
        let prompt = requests[0].last().unwrap().first_text_content().unwrap();
        let request_at = prompt.find(REQUEST_EXTRA).expect(prompt);
        let layer_at = prompt.find(LAYER_EXTRA).expect(prompt);
        assert!(request_at < layer_at, "{prompt}");
    }

    const KEEP_GOING: &str = "keep going after the summary";

    #[tokio::test]
    async fn manual_compact_steers_the_layers_words() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![Message::user("work".into())]);

        let (result, events) = summarize_steered(
            &provider,
            &mut history,
            None,
            before_answers(json!({ FIELD_CONTINUE: KEEP_GOING })),
        )
        .await;
        assert_eq!(result.unwrap(), DoneReason::Compact);

        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Steered { kind: SteerKind::CompactContinued, text } if text == KEEP_GOING
            )),
            "the TUI hears the layer's words: {events:?}"
        );
        assert_eq!(
            history.as_slice().last().unwrap().first_text_content(),
            Some(KEEP_GOING)
        );
    }

    #[tokio::test]
    async fn manual_compact_reports_the_summary() {
        let usage = TokenUsage {
            input: SUMMARISED_INPUT / 2,
            output: SUMMARY.len() as u64,
            ..Default::default()
        };
        let provider = MockProvider::new(vec![Ok(StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: SUMMARY.into(),
                }],
                ..Default::default()
            },
            usage,
            ..text_response(StopReason::EndTurn)
        })]);
        let mut history = History::new(vec![Message::user("work".into())]);

        let (result, events) =
            summarize_steered(&provider, &mut history, None, |_, _| Verdict::Unchanged).await;
        assert_eq!(result.unwrap(), DoneReason::Compact);

        let [
            AgentEvent::CompactionStarted,
            AgentEvent::TurnComplete(turn),
            AgentEvent::CompactionDone {
                context_size_after,
                context_window,
                summary,
                ..
            },
            AgentEvent::Done {
                usage: done_usage,
                context_size,
                context_window: done_window,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(summary, SUMMARY);
        assert_eq!(*done_usage, usage);
        assert_eq!(context_size, context_size_after);
        let session_window = default_model().context_window;
        assert_eq!(
            [turn.context_window, *context_window, *done_window],
            [session_window; 3]
        );
    }

    #[test_case(None, None, false, false ; "no_instructions")]
    #[test_case(Some(CONFIG_EXTRA), None, true, false ; "config_only")]
    #[test_case(None, Some(REQUEST_EXTRA), false, true ; "request_only")]
    #[test_case(Some(CONFIG_EXTRA), Some(REQUEST_EXTRA), true, true ; "both_kept")]
    #[test_case(Some(CONFIG_EXTRA), Some("   "), true, false ; "blank_request_ignored")]
    #[test_case(Some(" \n "), Some(REQUEST_EXTRA), false, true ; "blank_config_ignored")]
    fn summary_prompt_merges_instructions(
        config_extra: Option<&str>,
        request: Option<&str>,
        has_config: bool,
        has_request: bool,
    ) {
        let config = AgentConfig {
            compaction_instructions: config_extra.map(str::to_string),
            ..Default::default()
        };
        let prompt = summary_prompt(&config, request);

        assert!(prompt.starts_with(COMPACTION_USER));
        assert_eq!(
            prompt.len() > COMPACTION_USER.len(),
            has_config || has_request
        );
        assert_eq!(prompt.contains(CONFIG_EXTRA), has_config);
        assert_eq!(prompt.contains(REQUEST_EXTRA), has_request);
    }

    #[tokio::test]
    async fn compact_preparation_removes_orphan_result_and_tool_image() {
        use std::sync::Arc;

        use maki_providers::{ImageMediaType, ImageSource};

        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let image = ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, Arc::from("aGVsbG8=")),
        };
        let mut orphan = Message {
            role: Role::User,
            content: vec![tool_result("orphan"), image.clone()],
            ..Default::default()
        };
        orphan.content.push(ContentBlock::Text {
            text: "keep text".into(),
        });
        let chat_image = Message {
            role: Role::User,
            content: vec![image],
            ..Default::default()
        };
        let mut history = History::new(vec![orphan, chat_image]);
        summarize_history(&provider, &mut history, 0, None).await;

        let requests = provider.requests.lock().unwrap();
        let request = &requests[0];
        assert!(
            !request
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult { .. } | ContentBlock::Image { .. }
                ))
        );
        assert!(
            request
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, ContentBlock::Text { text } if text == "keep text"))
        );
        assert!(request.iter().flat_map(|message| &message.content).any(
            |block| matches!(block, ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
        ));
    }

    #[test_case(159_999, 0,       0,       0,      200_000, false ; "below_threshold")]
    #[test_case(160_000, 0,       0,       0,      200_000, true  ; "at_threshold")]
    #[test_case(100,     0,       0,       0,      100,     true  ; "tiny_context_window")]
    #[test_case(5_000,   165_000, 10_000,  0,      200_000, true  ; "cached_tokens_count_toward_overflow")]
    #[test_case(100_000, 0,       0,       80_000, 200_000, true  ; "output_tokens_count_toward_overflow")]
    #[test_case(262_144, 0,       0,       0,      262_144, true  ; "equal_context_and_max_output")]
    #[test_case(51_199,  0,       0,       0,      64_000,  false ; "small_window_below_scaled_threshold")]
    #[test_case(51_200,  0,       0,       0,      64_000,  true  ; "small_window_at_scaled_threshold")]
    // The output floor alone is the whole window here, so without a ceiling on
    // the reservation every one of these would overflow on an empty transcript.
    #[test_case(2_047,   0,       0,       0,      4_096,   false ; "llama_cpp_default_window_is_usable")]
    #[test_case(2_048,   0,       0,       0,      4_096,   true  ; "llama_cpp_default_window_still_compacts")]
    #[test_case(0,       0,       0,       0,      1_024,   false ; "an_empty_transcript_never_overflows")]
    fn overflow_detection(
        input: u64,
        cache_read: u64,
        cache_creation: u64,
        output: u64,
        ctx_window: u32,
        expected: bool,
    ) {
        let model = small_context_model(ctx_window);
        let usage = TokenUsage {
            input,
            output,
            cache_read,
            cache_creation,
            ..Default::default()
        };
        assert_eq!(
            is_overflow(usage.context_tokens(), &model, &AgentConfig::default()),
            expected
        );
    }

    #[test_case(CompactionBuffer::Tokens(10_000), 53_999, false ; "explicit_tokens_below")]
    #[test_case(CompactionBuffer::Tokens(10_000), 54_000, true  ; "explicit_tokens_honored")]
    #[test_case(CompactionBuffer::Percent(50),    32_000, true  ; "explicit_percent_at_threshold")]
    // A buffer under the output floor cannot leave a turn room to answer.
    #[test_case(CompactionBuffer::Tokens(1_000),  60_000, true  ; "buffer_below_the_output_floor_is_raised")]
    fn overflow_with_explicit_buffer(buffer: CompactionBuffer, input: u64, expected: bool) {
        let model = small_context_model(64_000);
        let config = AgentConfig {
            compaction_buffer: buffer,
            ..AgentConfig::default()
        };
        assert_eq!(is_overflow(input, &model, &config), expected);
    }

    #[test]
    fn strip_images_replaces_with_placeholder() {
        use maki_providers::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc"));
        let mut messages = vec![Message::user_with_images("hello".into(), vec![source])];
        strip_images(&mut messages);
        assert_eq!(messages[0].content.len(), 2);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
        );
        assert!(matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn strip_thinking_removes_thinking_blocks() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: Some("sig".into()),
                    reasoning_id: None,
                },
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
            ],
            ..Default::default()
        }];
        strip_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[test_case(OLD_RESULT.len() + NEW_RESULT.len(), &[OLD_RESULT, NEW_RESULT] ; "whole_tail_fits")]
    #[test_case(NEW_RESULT.len(), &[TOOL_RESULT_PLACEHOLDER, NEW_RESULT] ; "only_newest_fits")]
    #[test_case(NEW_RESULT.len() - 1, &[OLD_RESULT, TOOL_RESULT_PLACEHOLDER] ; "oversized_newest_spares_the_older_ones")]
    fn collapse_tool_results_budgets_the_tail(budget: usize, expected: &[&str]) {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::tool_result("t1", OLD_RESULT, false),
                ContentBlock::tool_result("t2", NEW_RESULT, false),
                ContentBlock::Text {
                    text: KEPT_TEXT.into(),
                },
            ],
            ..Default::default()
        }];
        collapse_tool_results(&mut messages, budget);

        for (block, expected) in messages[0].content.iter().zip(expected) {
            assert!(
                matches!(block, ContentBlock::ToolResult { content, .. } if content == expected)
            );
        }
        assert!(
            matches!(&messages[0].content[2], ContentBlock::Text { text } if text == KEPT_TEXT)
        );
    }

    /// The layer flips the host's pick, and its index past the end is dropped
    /// rather than trusted.
    #[tokio::test]
    async fn prepare_layer_picks_what_collapses() {
        const TOOL: &str = "read";
        const OUT_OF_RANGE: usize = 99;
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::tool_use("t1", TOOL, json!({})),
                    ContentBlock::tool_use("t2", TOOL, json!({})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::tool_result("t1", OLD_RESULT, false),
                    ContentBlock::tool_result("t2", NEW_RESULT, false),
                ],
                ..Default::default()
            },
        ];
        let registry = ToolRegistry::new();
        let seen = script(&registry, |_, _| {
            Verdict::Replaced(json!({ FIELD_COLLAPSE: [2, OUT_OF_RANGE] }))
        });
        let model = default_model();
        let cancel = CancelToken::none();

        prepare_collapse(
            &test_hooks(&registry, None, &model, &cancel),
            &mut messages,
            NEW_RESULT.len(),
        )
        .await;

        let (slot, value) = seen.lock().unwrap()[0].clone();
        assert_eq!(slot, AgentSlot::CompactPrepare);
        assert_eq!(value[FIELD_COLLAPSE], json!([1]));
        assert_eq!(value["results"][0]["tool"], TOOL);
        let contents: Vec<&str> = messages[1]
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::ToolResult { content, .. } => content.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(contents, [OLD_RESULT, TOOL_RESULT_PLACEHOLDER]);
    }

    fn tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(id, "bash", serde_json::json!({}))],
            ..Default::default()
        }
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::tool_result(id, "output", false)
    }

    #[track_caller]
    fn assert_tool_results_have_calls(messages: &[Message]) {
        for (index, message) in messages.iter().enumerate() {
            for block in &message.content {
                let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                    continue;
                };
                assert!(matches!(message.role, Role::User));
                assert!(index > 0);
                assert!(
                    messages[index - 1]
                        .tool_uses()
                        .any(|(id, _, _)| id == tool_use_id)
                );
            }
        }
    }

    #[tokio::test]
    async fn compact_history_retries_without_reproduced_orphan() {
        const TOOL_USE_ID: &str = "call_dMZDTpEfz2JxMvFbqFHua1Zy";

        let provider = MockProvider::new(vec![
            Err(overflow_error()),
            Err(overflow_error()),
            Ok(text_response(StopReason::EndTurn)),
        ]);
        let mut history = History::new(vec![
            Message::user("request".into()),
            tool_use(TOOL_USE_ID),
            Message {
                role: Role::User,
                content: vec![tool_result(TOOL_USE_ID)],
                ..Default::default()
            },
            Message::user("prompt".into()),
        ]);
        summarize_history(&provider, &mut history, 0, None).await;

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0]
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == TOOL_USE_ID)));
        // Attempt 1 only collapses the result, attempt 2 drops the round.
        assert!(requests[1]
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER)));
        assert!(
            !requests[2]
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        );
    }

    const NO_COLLAPSE_MSG: &str =
        "with no tool result to collapse the retry must prune instead of resending";

    #[tokio::test]
    async fn compact_history_prunes_when_there_is_nothing_to_collapse() {
        let provider = MockProvider::new(vec![
            Err(overflow_error()),
            Ok(text_response(StopReason::EndTurn)),
        ]);
        let mut history = History::new(vec![
            Message::user("first".into()),
            Message::user("second".into()),
        ]);
        summarize_history(&provider, &mut history, 0, None).await;

        let requests = provider.requests.lock().unwrap();
        assert!(requests[1].len() < requests[0].len(), "{NO_COLLAPSE_MSG}");
    }

    /// OpenCode Go rejects requests without `x-opencode-session`, so the
    /// summariser must ride the conversation's id like every other turn.
    #[tokio::test]
    async fn compact_history_sends_the_conversations_session_id() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![Message::user("work".into())]);
        let session = SessionRef::generate();
        summarize_history(&provider, &mut history, 0, Some(&session)).await;

        let sessions = provider.sessions.lock().unwrap();
        assert_eq!(sessions[0].as_deref(), Some(session.as_str()));
    }

    const CARRIED: &str = "answer this next";
    const CARRY_UNSENT_MSG: &str = "carried input is not the summariser's to read";
    const CARRY_KEPT_MSG: &str = "carried input must outlive the summary verbatim";

    #[tokio::test]
    async fn compact_history_carries_the_tail_past_the_summary() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![
            Message::user("first".into()),
            Message::user(CARRIED.into()),
        ]);
        summarize_history(&provider, &mut history, 1, None).await;

        let carried = |messages: &[Message]| {
            messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, ContentBlock::Text { text } if text == CARRIED))
        };
        assert!(
            !carried(&provider.requests.lock().unwrap()[0]),
            "{CARRY_UNSENT_MSG}"
        );
        assert!(carried(history.as_slice()), "{CARRY_KEPT_MSG}");
    }

    #[tokio::test]
    async fn compaction_keeps_observation_before_dependent_reply() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![
            Message::observation("[monitor] build failed".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I will fix it".into(),
                }],
                ..Default::default()
            },
        ]);
        summarize_history(&provider, &mut history, 0, None).await;

        let requests = provider.requests.lock().unwrap();
        assert!(requests[0][0].is_observation());
        assert!(matches!(requests[0][1].role, Role::Assistant));
    }

    #[test]
    fn truncate_oldest_round_preserves_text_beside_orphan() {
        let mut messages = vec![
            Message::user("request".into()),
            tool_use("expected"),
            Message {
                role: Role::User,
                content: vec![
                    tool_result("mismatched"),
                    ContentBlock::Text {
                        text: "keep me".into(),
                    },
                ],
                ..Default::default()
            },
            Message::user("prompt".into()),
        ];

        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);

        assert_eq!(messages.len(), 2);
        assert!(
            matches!(&messages[0].content[..], [ContentBlock::Text { text }] if text == "keep me")
        );
        assert_tool_results_have_calls(&messages);
    }

    #[test]
    fn truncate_oldest_round_removes_single_user_message() {
        let mut messages = vec![
            Message::user("first".into()),
            Message::user("second".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "second"));
    }

    #[test]
    fn truncate_oldest_round_removes_assistant_tool_pair() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("t1", "output", false)],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_removes_assistant_without_matching_tool_result() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message::user("no tool result".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "no tool result")
        );
    }

    #[test]
    fn truncate_oldest_round_noop_on_single_message() {
        let mut messages = vec![Message::user("only".into())];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn truncate_oldest_round_removes_plain_assistant() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_consecutive_assistants_drains_until_user() {
        // [User, Assistant(no tools), Assistant(tools), User(results)] drains 2,
        // leaving Assistant-first — keep draining until first is User.
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "plain reply".into(),
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", "bash", serde_json::json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("t1", "output", false)],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_tool_results_have_calls(&messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[..], [ContentBlock::Text { text }] if text == "keep me")
        );
    }
}
