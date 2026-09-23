use maki_providers::ProviderSession;
use std::time::{Duration, Instant};

use maki_providers::provider::Provider;
use maki_providers::retry::{RetryPolicy, RetryState};
use maki_providers::{
    ContentBlock, ContextGauge, Message, Model, Overflow, ProviderEvent, RequestOptions,
    StreamResponse, estimate_prompt_tokens,
};
use serde_json::Value;
use tracing::warn;

use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender};

const FUNCTIONS_PREFIX: &str = "functions.";
/// Floor for the budget below, never applied above the model's own cap.
/// Anthropic derives the thinking budget from `max_tokens` (half of it, at
/// least 1024) and rejects a request where the two are equal, so flooring the
/// cap all the way down to 1024 would trade an overflow for a 400.
const MIN_OUTPUT_TOKENS: u32 = 4096;
/// Each round halves the ask, or lands exactly when the server quoted its own
/// numbers, so two is enough for both. Past that the prompt is the problem.
const MAX_BUDGET_RETRIES: u32 = 2;
/// `AgentError::retry_message` falls back to the raw `API error (400): ...`
/// body for an overflow, which is a paragraph of server arithmetic in a
/// one-line status bar. The only part the user can act on is that the same
/// turn is on its way back, asking for less.
const BUDGET_RETRY_MESSAGE: &str = "output budget too large, retrying smaller";
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

/// The `max_tokens` one turn asks for.
///
/// Deliberately not "everything the window can spare": that ties `max_tokens`
/// to a prompt estimate, and an estimate that reads low buys a rejection from
/// every server enforcing `prompt + max_tokens <= context_window`. A turn asks
/// for what a turn needs, so the size the server checks is one maki chose.
///
/// Thinking is the exception, and not a small one. A dialect spends at most
/// half its `max_tokens` on thinking ([`Model::thinking_ceiling`]), so an
/// effort level the user picked arrives cut down unless twice its budget fits
/// here. On a high level that is most of the model's output cap.
///
/// The window clamp at the end is a backstop. Compaction reserves [`min_output`]
/// where the window can spare it ([`compaction::reserved`]), so a session that
/// compacts on schedule only meets it when the thinking reservation is what
/// crowds the window, and then the ceiling walks the thinking back down.
///
/// A model that declares no cap still gets a budget: "let the provider pick" is
/// unbounded on the servers that most need bounding (llama.cpp hands over the
/// rest of the window), and a size maki never set is one [`shrunk_budget`]
/// cannot climb down from.
///
/// [`compaction::reserved`]: super::compaction::reserved
fn planned_output(model: &Model, opts: &RequestOptions, budget: u32, prompt: u32) -> u32 {
    let thinking_room = opts
        .thinking
        .reserved_thinking(model)
        .map_or(0, |n| n.saturating_mul(2));
    budget
        .max(thinking_room)
        .min(model.max_output_tokens.unwrap_or(u32::MAX))
        .min(model.context_window.saturating_sub(prompt))
        .max(min_output(model))
}

/// Smallest cap a turn may be trimmed to, never above what the model declared,
/// since it would reject a number it never offered. Compaction reserves the
/// same floor, so a session that compacts on schedule never reaches it.
pub(super) fn min_output(model: &Model) -> u32 {
    MIN_OUTPUT_TOKENS.min(model.max_output_tokens.unwrap_or(MIN_OUTPUT_TOKENS))
}

/// The budget to try after a server refused the last one.
///
/// A server that quoted its own numbers already did the arithmetic, so the next
/// attempt fits exactly. Without them, halving converges in a round or two.
/// `None` means nothing above `floor` is small enough and the prompt itself has
/// to give, which is the caller's problem.
fn shrunk_budget(current: u32, overflow: Overflow, window: u32, floor: u32) -> Option<u32> {
    let Overflow::Budget { prompt, limit } = overflow else {
        return None;
    };
    // A quote landing at or above the budget just refused was not the
    // arithmetic the server ran: `window` is maki's own number, and providers
    // that resolve limits per request (catalog, opencode) override it on the
    // way out. Halving is the honest answer then.
    let next = match prompt.map(|p| limit.unwrap_or(window).saturating_sub(p)) {
        Some(exact) if exact < current => exact,
        _ => current / 2,
    };
    (next >= floor && next < current).then_some(next)
}

/// One request, with everything that describes it. A struct and not a dozen
/// positional arguments, because the retry loop below re-sends it with one
/// field changed, and a caller swapping two `&str` would not be caught.
pub(crate) struct StreamRequest<'a> {
    pub provider: &'a dyn Provider,
    pub model: &'a Model,
    pub messages: &'a [Message],
    pub system: &'a str,
    pub tools: &'a Value,
    pub opts: RequestOptions,
    /// Output tokens this kind of turn may generate. A summary needs far less
    /// than a coding turn, so the caller decides.
    pub output_budget: u32,
    pub session_id: Option<&'a ProviderSession>,
    pub retry: RetryPolicy,
}

/// `gauge` is `None` for a request whose messages are not the session's, as
/// compaction's stripped and collapsed rewrite is: neither its size nor the
/// count it comes back with says anything about the session.
pub(crate) async fn stream_with_retry(
    req: StreamRequest<'_>,
    mut gauge: Option<&mut ContextGauge>,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<StreamResponse, StreamError> {
    let StreamRequest {
        provider,
        model,
        messages,
        system,
        tools,
        opts,
        output_budget,
        session_id,
        retry,
    } = req;
    let opts = opts.clamped(model);
    // The session's own size wins where it is larger, being a count a provider
    // made rather than a byte estimate.
    let measured = gauge.as_deref().map_or(0, ContextGauge::size);
    let prompt = estimate_prompt_tokens(messages, system, tools).max(measured);
    let floor = min_output(model);
    let mut budget = planned_output(model, &opts, output_budget, prompt);
    let mut budget_retries = 0;
    // Counted here rather than off `retry`, which neither a budget retry nor a
    // rotation spends, so the number otel and the status bar show never repeats
    // or walks backwards.
    let mut attempt = 0;
    // Rebuilding images a provider would refuse can take real time on the
    // first request of a session full of screenshots, and it all happens
    // before anything below can observe a cancel.
    let adapted = cancel
        .race(maki_providers::adapt_images_for_model(model, messages))
        .await
        .map_err(|_| StreamError::Cancelled {
            streamed: String::new(),
        })?;
    let messages = &*adapted;
    let mut retry = RetryState::new(retry, provider.keys().map_or(1, |keys| keys.key_count()));
    loop {
        // The turn budget is all that moves. What the model declares stays put:
        // the thinking a request may spend is read off the `max_tokens` this
        // produces, so the two can never disagree, whatever a provider
        // rewrites on the way out.
        let fitted = model.with_turn_output(budget);
        let model = &fitted;
        let started = Instant::now();
        let (ptx, prx) = flume::unbounded();
        let forwarder = smol::spawn({
            let event_tx = event_tx.clone();
            async move { forward_provider_events(prx, &event_tx).await }
        });
        // `race` checks the token before it polls, so a run cancelled while a
        // retry slept on a server `Retry-After` never pays for the attempt it
        // woke up to make.
        let result = cancel
            .race(provider.stream_message(
                model,
                messages,
                system,
                tools,
                &ptx,
                opts.clone(),
                session_id,
            ))
            .await
            .unwrap_or(Err(AgentError::Cancelled));
        drop(ptx);
        let streamed = forwarder.await;
        match result {
            Ok(mut r) => {
                canonicalize_tool_names(&mut r.message);
                emit_api_request(model, &r, &opts, started.elapsed());
                if let Some(gauge) = gauge.as_deref_mut() {
                    gauge.record(r.usage.total_input());
                }
                return Ok(r);
            }
            Err(AgentError::Cancelled) => return Err(StreamError::Cancelled { streamed }),
            Err(e) => {
                attempt += 1;
                emit_api_error(model, &e, attempt, started.elapsed());
                // Rotation is decided above anything `retry_kind()` says: a
                // fresh key and a delay cure different problems. A 401 or a 403
                // is dead for the key that produced it, fine for the next one,
                // and carries no retry kind at all, while waiting is the only
                // answer for a throttled account. So the pool gets walked
                // first, on nobody's budget.
                let rotated = e.should_rotate_key()
                    && retry.book_rotation()
                    && provider.keys().is_some_and(|keys| keys.rotate());
                // Every arm below only decides what the next attempt costs, so
                // the one tail after them is the only way round the loop.
                let (message, delay) = if rotated {
                    (e.retry_message(), Duration::ZERO)
                } else if let Some(kind) = e.retry_kind() {
                    let Some(delay) = retry.next_delay(kind, e.retry_after()) else {
                        return Err(e.into());
                    };
                    (e.retry_message(), delay)
                } else {
                    // A budget overflow is the one rejection maki caused itself,
                    // by asking for more output than the prompt left room for.
                    // Asking for less costs nothing, so it happens here instead
                    // of falling through to the caller, whose only remedy is to
                    // summarize the session away.
                    let Some(
                        overflow @ Overflow::Budget {
                            prompt: measured, ..
                        },
                    ) = e.overflow()
                    else {
                        return Err(e.into());
                    };
                    // The server counted the prompt maki could only estimate.
                    if let Some(gauge) = gauge.as_deref_mut() {
                        gauge.record(measured.unwrap_or(0));
                    }
                    let Some(next) = (budget_retries < MAX_BUDGET_RETRIES)
                        .then(|| shrunk_budget(budget, overflow, model.context_window, floor))
                        .flatten()
                    else {
                        return Err(e.into());
                    };
                    budget_retries += 1;
                    warn!(
                        model = %model.id,
                        from = budget,
                        to = next,
                        measured_prompt = measured,
                        "output budget did not fit the window, retrying smaller"
                    );
                    budget = next;
                    (BUDGET_RETRY_MESSAGE.to_owned(), Duration::ZERO)
                };
                // Every way back around the loop passes here, and it has to:
                // `AgentEvent::Retry` is the view's only cue to drop what the
                // failed attempt streamed (`stream_reset`). A path that looped
                // in silence would leave that text for the next attempt to be
                // appended to.
                let delay_ms = delay.as_millis() as u64;
                warn!(attempt, delay_ms, rotated, error = %e, "retryable, will retry");
                event_tx.send(AgentEvent::Retry {
                    attempt,
                    message,
                    delay_ms,
                })?;
                if !delay.is_zero() {
                    let _ = cancel.race(smol::Timer::after(delay)).await;
                }
            }
        }
    }
}

fn emit_api_request(model: &Model, r: &StreamResponse, opts: &RequestOptions, took: Duration) {
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
    use std::sync::Mutex;

    use maki_providers::{
        Effort, KeyHeader, KeyPool, KeyRotation, ResolvedAuth, Role, ThinkingConfig, TokenUsage,
    };
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const SECRET_BODY: &str = "messages.0.content: \"my private prompt\", key sk-abc";
    const WINDOW: u32 = 262_144;
    /// The shape that started all this: the declared output cap is the whole
    /// window, so "ask for what fits" asks for everything.
    const HUGE_MAX_OUTPUT: u32 = WINDOW;
    const SMALL_MAX_OUTPUT: u32 = 2_048;
    /// Half the window, as the real models declare it: `claude-sonnet-4` offers
    /// 128k of output against a 200k window. A cap equal to the whole window
    /// leaves no prompt small enough for `max` effort to fit beside it.
    const DECLARED_MAX_OUTPUT: u32 = WINDOW / 2;
    const TURN_BUDGET: u32 = 32_768;
    const SMALL_PROMPT: u32 = 1_000;
    const EXPLICIT_THINKING: u32 = 30_000;
    /// Less room than `TURN_BUDGET` wants.
    const CROWDED_ROOM: u32 = 10_000;
    const CROWDING_PROMPT: u32 = WINDOW - CROWDED_ROOM;

    fn model_with(max_output_tokens: Option<u32>) -> Model {
        let mut model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        model.context_window = WINDOW;
        model.max_output_tokens = max_output_tokens;
        model
    }

    fn opts_with(thinking: ThinkingConfig) -> RequestOptions {
        RequestOptions {
            thinking,
            fast: false,
            prompt_cache_key: None,
        }
    }

    // The budget does not move with the prompt, so no estimate error can make
    // the request malformed.
    #[test_case(Some(HUGE_MAX_OUTPUT), SMALL_PROMPT, TURN_BUDGET; "budget_ignores_a_small_prompt")]
    #[test_case(Some(HUGE_MAX_OUTPUT), WINDOW / 2, TURN_BUDGET; "budget_ignores_a_large_prompt")]
    #[test_case(Some(SMALL_MAX_OUTPUT), SMALL_PROMPT, SMALL_MAX_OUTPUT; "the_model_cap_still_wins")]
    // Backstop, reached only once compaction has been turned off or outrun.
    #[test_case(Some(HUGE_MAX_OUTPUT), CROWDING_PROMPT, CROWDED_ROOM; "a_crowded_window_trims_the_budget")]
    #[test_case(Some(HUGE_MAX_OUTPUT), WINDOW + 1, MIN_OUTPUT_TOKENS; "prompt_over_window_floors_at_minimum")]
    #[test_case(Some(SMALL_MAX_OUTPUT), WINDOW + 1, SMALL_MAX_OUTPUT; "floor_never_exceeds_the_model_cap")]
    // A size maki never set is one it cannot climb down from later.
    #[test_case(None, SMALL_PROMPT, TURN_BUDGET; "an_undeclared_cap_still_gets_a_budget")]
    fn the_output_budget_is_flat(
        max_output_tokens: Option<u32>,
        prompt_tokens: u32,
        expected: u32,
    ) {
        assert_eq!(
            planned_output(
                &model_with(max_output_tokens),
                &opts_with(ThinkingConfig::Off),
                TURN_BUDGET,
                prompt_tokens
            ),
            expected
        );
    }

    /// The declared model and the one a request would carry, checked on the way
    /// out against the invariant that has to hold for every one of them:
    /// whatever the turn or a provider does to the output cap, the thinking is
    /// at most half the number on the wire, so the answer always has room and
    /// no dialect is handed a budget its own `max_tokens` refuses.
    fn fitted_with(thinking: ThinkingConfig, prompt: u32) -> (Model, Model) {
        let declared = model_with(Some(DECLARED_MAX_OUTPUT));
        let budget = planned_output(&declared, &opts_with(thinking), TURN_BUDGET, prompt);
        let fitted = declared.with_turn_output(budget);

        let asked = fitted.output_tokens().expect("a budget is always set");
        assert!(
            thinking.request_thinking(&fitted).unwrap_or(0) * 2 <= asked,
            "{thinking} thinking does not fit under a {asked} token cap"
        );
        (declared, fitted)
    }

    /// The regression this half of the fix exists for. A dialect can only spend
    /// half its `max_tokens` on thinking, so a turn budget that ignored the
    /// effort level cut `high` on a 64k-cap model from 19200 thinking tokens to
    /// 9830. The turn asks for room instead, and the level is untouched.
    #[test_case(ThinkingConfig::Effort(Effort::Max) ; "the_top_effort_level")]
    #[test_case(ThinkingConfig::Budget(EXPLICIT_THINKING) ; "an_explicit_budget")]
    fn a_turn_budget_never_rescales_the_thinking_it_makes_room_for(thinking: ThinkingConfig) {
        let (declared, fitted) = fitted_with(thinking, SMALL_PROMPT);

        assert_eq!(
            thinking.request_thinking(&fitted),
            thinking.reserved_thinking(&declared)
        );
    }

    /// A window too full to hold the thinking the effort level asked for. The
    /// reservation used to ignore the window and ask for `2 x thinking` anyway,
    /// which Anthropic refuses with `input length and max_tokens exceed context
    /// limit`, and maki answered a refusal it had caused itself by compacting.
    /// On a 200k window that hit every `high` turn past 123k of prompt, well
    /// under the 160k where compaction was due.
    #[test_case(ThinkingConfig::Effort(Effort::Max), CROWDING_PROMPT ; "a_sliver_of_room_left")]
    #[test_case(ThinkingConfig::Effort(Effort::Max), WINDOW + 1 ; "no_room_left_at_all")]
    #[test_case(ThinkingConfig::Budget(EXPLICIT_THINKING), WINDOW + 1 ; "an_explicit_budget_gives_too")]
    fn a_crowded_window_cuts_the_thinking_instead_of_the_request(
        thinking: ThinkingConfig,
        prompt: u32,
    ) {
        let (declared, fitted) = fitted_with(thinking, prompt);
        let asked = fitted.output_tokens().expect("a budget is always set");
        let room = WINDOW.saturating_sub(prompt);

        assert!(
            asked <= room.max(min_output(&declared)),
            "asked for {asked} of output with {room} left in the window"
        );
        assert!(
            thinking.request_thinking(&fitted) < thinking.reserved_thinking(&declared),
            "the thinking has to give where the window cannot house it"
        );
    }

    const SERVER_LIMIT: u32 = 200_000;
    const SERVER_PROMPT: u32 = 180_000;

    const UNQUOTED: Overflow = Overflow::Budget {
        prompt: None,
        limit: None,
    };

    #[test_case(
        TURN_BUDGET,
        Overflow::Budget { prompt: Some(SERVER_PROMPT), limit: Some(SERVER_LIMIT) },
        Some(SERVER_LIMIT - SERVER_PROMPT)
        ; "quoted_numbers_land_exactly"
    )]
    #[test_case(TURN_BUDGET, UNQUOTED, Some(TURN_BUDGET / 2) ; "otherwise_halve")]
    // Our window is a fallback the provider overrides, so the subtraction
    // comes out above what was just refused and says nothing.
    #[test_case(
        TURN_BUDGET,
        Overflow::Budget { prompt: Some(SMALL_PROMPT), limit: None },
        Some(TURN_BUDGET / 2)
        ; "a_quote_that_cannot_be_the_real_arithmetic_falls_back_to_halving"
    )]
    // No budget is small enough, so the transcript is what has to give.
    #[test_case(
        TURN_BUDGET,
        Overflow::Budget { prompt: Some(WINDOW), limit: None },
        None
        ; "a_full_window_cannot_be_fixed_by_shrinking"
    )]
    // Halving below the floor would trade the overflow for a 400 on the very
    // same request, so the retrying stops here.
    #[test_case(MIN_OUTPUT_TOKENS, UNQUOTED, None ; "the_floor_is_the_last_stop")]
    #[test_case(TURN_BUDGET, Overflow::Prompt, None ; "a_prompt_overflow_is_not_ours_to_retry")]
    fn shrinking_answers_what_the_server_said(
        current: u32,
        overflow: Overflow,
        expected: Option<u32>,
    ) {
        assert_eq!(
            shrunk_budget(current, overflow, WINDOW, MIN_OUTPUT_TOKENS),
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
        let error = AgentError::api(400, SECRET_BODY);
        let reported = error_description(&error);
        assert!(!reported.contains("private"));
        assert_eq!(reported, "API error (400)");
    }

    /// The kind of server this module exists for: it enforces
    /// `prompt + max_tokens <= window` and counts the prompt with its own
    /// tokenizer, which reads higher than maki's byte estimate. Any budget
    /// derived from that estimate is too big by construction.
    struct StrictServer {
        window: u32,
        requests: Mutex<Vec<u32>>,
    }

    impl Provider for StrictServer {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a ProviderSession>,
        ) -> maki_providers::provider::BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let prompt = (estimate_prompt_tokens(messages, system, tools) as f32
                    * UNDERCOUNT_FACTOR) as u32;
                let asked = model.output_tokens().unwrap_or(0);
                self.requests.lock().unwrap().push(asked);
                if prompt + asked > self.window {
                    return Err(AgentError::api(
                        400,
                        format!(
                            "This model's maximum context length is {} tokens. However, you requested {} tokens ({prompt} in the messages, {asked} in the completion).",
                            self.window,
                            prompt + asked
                        ),
                    ));
                }
                Ok(StreamResponse {
                    message: Message::user("ok".into()),
                    usage: TokenUsage {
                        input: prompt,
                        ..Default::default()
                    },
                    stop_reason: Some(maki_providers::StopReason::EndTurn),
                })
            })
        }

        fn list_models(
            &self,
        ) -> maki_providers::provider::BoxFuture<
            '_,
            Result<Vec<maki_providers::ModelInfo>, AgentError>,
        > {
            Box::pin(async { unimplemented!() })
        }
    }

    const UNDERCOUNT_FACTOR: f32 = 1.25;
    const TRANSCRIPT_BYTES: usize = 200_000;
    const STRICT_WINDOW: u32 = 262_144;
    /// Roomy enough for the transcript, too tight for the transcript plus a
    /// whole turn budget.
    const CROWDED_WINDOW: u32 = 80_000;

    fn transcript() -> Vec<Message> {
        vec![Message::user("x".repeat(TRANSCRIPT_BYTES))]
    }

    fn strict_model(window: u32) -> Model {
        let mut model = model_with(Some(window));
        model.context_window = window;
        model
    }

    async fn send(
        provider: &dyn Provider,
        model: &Model,
        gauge: Option<&mut ContextGauge>,
        retry: RetryPolicy,
    ) -> (Result<StreamResponse, StreamError>, Vec<AgentEvent>) {
        let (tx, rx) = flume::unbounded();
        let result = stream_with_retry(
            StreamRequest {
                provider,
                model,
                messages: &transcript(),
                system: "",
                tools: &json!([]),
                opts: RequestOptions::default(),
                output_budget: TURN_BUDGET,
                session_id: None,
                retry,
            },
            gauge,
            &EventSender::new(tx, 0),
            &CancelToken::none(),
        )
        .await;
        (
            result,
            rx.try_iter().map(|envelope| envelope.event).collect(),
        )
    }

    fn retries_of(events: &[AgentEvent]) -> Vec<(u32, &str, u64)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Retry {
                    attempt,
                    message,
                    delay_ms,
                } => Some((*attempt, message.as_str(), *delay_ms)),
                _ => None,
            })
            .collect()
    }

    const FIRST_ATTEMPT: u32 = 1;
    const NO_WAIT: u64 = 0;

    /// A race wakes from whichever side gets there first, so racing the token
    /// against the request still sent it about half the time. `race` checks the
    /// token before it polls at all.
    #[test]
    fn a_cancelled_run_is_never_billed_for_a_request() {
        smol::block_on(async {
            let server = StrictServer {
                window: STRICT_WINDOW,
                requests: Mutex::default(),
            };
            let (trigger, cancel) = CancelToken::new();
            trigger.cancel();
            let (tx, _rx) = flume::unbounded();

            let result = stream_with_retry(
                StreamRequest {
                    provider: &server,
                    model: &strict_model(STRICT_WINDOW),
                    messages: &transcript(),
                    system: "",
                    tools: &json!([]),
                    opts: RequestOptions::default(),
                    output_budget: TURN_BUDGET,
                    session_id: None,
                    retry: RetryPolicy::default(),
                },
                None,
                &EventSender::new(tx, 0),
                &cancel,
            )
            .await;

            assert!(matches!(result, Err(StreamError::Cancelled { .. })));
            assert!(
                server.requests.lock().unwrap().is_empty(),
                "a cancelled run reached the provider"
            );
        });
    }

    /// `AgentEvent::Retry` is the only thing that makes the view drop what the
    /// refused attempt already streamed, so a budget retry that shrinks the ask
    /// silently leaves the dead attempt's text on screen for the answer that
    /// replaces it to be appended to.
    #[test_case(STRICT_WINDOW, &[] ; "a_request_that_fits_reports_nothing")]
    #[test_case(CROWDED_WINDOW, &[(FIRST_ATTEMPT, BUDGET_RETRY_MESSAGE, NO_WAIT)] ; "a_shrunk_budget_is_reported_like_any_other_retry")]
    fn every_way_round_the_loop_tells_the_view_to_start_over(
        window: u32,
        expected: &[(u32, &str, u64)],
    ) {
        smol::block_on(async {
            let server = StrictServer {
                window,
                requests: Mutex::default(),
            };
            let mut gauge = ContextGauge::default();

            let (result, events) = send(
                &server,
                &strict_model(window),
                Some(&mut gauge),
                RetryPolicy::default(),
            )
            .await;

            result.expect("a shrunk budget is answered");
            assert_eq!(retries_of(&events), expected);
        });
    }

    #[test_case(STRICT_WINDOW, 1 ; "a_window_sized_cap_still_leaves_room_for_the_prompt")]
    // A retry is cheap and compaction is not, so a budget that genuinely does
    // not fit comes back smaller, with nothing summarized away to get there.
    #[test_case(CROWDED_WINDOW, 2 ; "a_budget_that_does_not_fit_is_retried_smaller")]
    fn a_strict_server_is_answered_without_dropping_context(window: u32, attempts: usize) {
        smol::block_on(async {
            let server = StrictServer {
                window,
                requests: Mutex::default(),
            };
            let mut gauge = ContextGauge::default();

            let response = send(
                &server,
                &strict_model(window),
                Some(&mut gauge),
                RetryPolicy::default(),
            )
            .await
            .0
            .expect("a flat budget leaves the window room for the prompt");

            let asks = server.requests.lock().unwrap().clone();
            assert_eq!(asks.len(), attempts, "asked for {asks:?}");
            assert!(
                asks[0] <= TURN_BUDGET,
                "the first ask is a turn budget, not the window"
            );
            assert!(
                asks.windows(2).all(|pair| pair[1] < pair[0]),
                "every retry asks for less than the ask that was refused"
            );
            assert_eq!(
                gauge.size(),
                response.usage.input,
                "the session keeps the server's count, not the estimate under it"
            );
        });
    }

    const POOL_KEYS: [&str; 3] = ["sk-first", "sk-second", "sk-third"];
    const FULL_POOL: usize = POOL_KEYS.len();
    const SINGLE_KEY: usize = 1;
    const RATE_LIMITED: u16 = 429;
    const UNAUTHORIZED: u16 = 401;
    const FORBIDDEN: u16 = 403;
    const REJECTED_BODY: &str = "this key is done";
    const POOLED_SLUG: &str = "pooled-test-server";

    /// Every rotation case runs with nothing to spend, so any path that reaches
    /// a budget fails on the spot: that the walk happens anyway is the proof
    /// that a fresh key costs neither a retry nor a delay, and that no case
    /// sleeps.
    fn no_budget() -> RetryPolicy {
        RetryPolicy {
            max_retries: 0,
            max_timeout_retries: 0,
            ..RetryPolicy::default()
        }
    }

    /// A server behind a key pool: it rejects with `status` until the key in
    /// `relents_for` is the current one, and records the key every request was
    /// made with, so a walk shows up as distinct keys rather than as a count.
    struct PooledServer {
        pool: KeyPool,
        auth: Mutex<ResolvedAuth>,
        status: u16,
        relents_for: Option<&'static str>,
        seen: Mutex<Vec<String>>,
    }

    impl PooledServer {
        fn new(keys: usize, status: u16, relents_for: Option<&'static str>) -> Self {
            let pool = KeyPool::from_keys(POOL_KEYS[..keys].iter().map(|k| (*k).into()).collect());
            let auth = ResolvedAuth::bearer(POOLED_SLUG, pool.current()).unwrap();
            Self {
                pool,
                auth: Mutex::new(auth),
                status,
                relents_for,
                seen: Mutex::default(),
            }
        }

        fn keys_seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Provider for PooledServer {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a ProviderSession>,
        ) -> maki_providers::provider::BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let key = self.pool.current().to_owned();
                let relents = self.relents_for == Some(key.as_str());
                self.seen.lock().unwrap().push(key);
                if !relents {
                    return Err(AgentError::api(self.status, REJECTED_BODY));
                }
                Ok(StreamResponse {
                    message: Message::user("ok".into()),
                    usage: TokenUsage::default(),
                    stop_reason: Some(maki_providers::StopReason::EndTurn),
                })
            })
        }

        fn list_models(
            &self,
        ) -> maki_providers::provider::BoxFuture<
            '_,
            Result<Vec<maki_providers::ModelInfo>, AgentError>,
        > {
            Box::pin(async { unimplemented!() })
        }

        fn keys(&self) -> Option<KeyRotation<'_>> {
            Some(KeyRotation::new(&self.pool, &self.auth, KeyHeader::Bearer))
        }
    }

    /// Sends one turn through the pool and checks what the loop's tail owes the
    /// view for every rotation it took: a `Retry` event at all, so the failed
    /// attempt's half printed text is dropped instead of appended to, and a
    /// zero delay, because a fresh key waits for nothing.
    async fn send_pooled(server: &PooledServer) -> Result<StreamResponse, StreamError> {
        let (result, events) = send(server, &strict_model(STRICT_WINDOW), None, no_budget()).await;

        let retries = retries_of(&events);
        assert_eq!(
            retries.len(),
            server.keys_seen().len() - 1,
            "every rotation announces itself: {events:?}"
        );
        for (nth, (attempt, _, delay_ms)) in retries.iter().enumerate() {
            assert_eq!(*attempt as usize, nth + 1, "attempts count up from one");
            assert_eq!(*delay_ms, NO_WAIT, "a rotation waits for nothing");
        }
        result
    }

    /// The reported regression: a 429 on the first key used to spend the rate
    /// limit budget before anything rotated, so a pool whose budget was zero
    /// never reached its other keys at all.
    #[test]
    fn a_fresh_key_is_tried_without_spending_the_retry_budget() {
        smol::block_on(async {
            let server = PooledServer::new(FULL_POOL, RATE_LIMITED, Some(POOL_KEYS[FULL_POOL - 1]));

            send_pooled(&server)
                .await
                .expect("the last key in the pool answers");

            assert_eq!(
                server.keys_seen(),
                POOL_KEYS,
                "the walk tries each key once, in the pool's order"
            );
        });
    }

    /// A 401 and a 403 carry no retry kind at all, which is why rotation is
    /// decided above `retry_kind()`: the key that produced one is dead and the
    /// next one may be fine. Whatever the status, the walk ends when the pool
    /// does.
    #[test_case(RATE_LIMITED, FULL_POOL  ; "a_rate_limit_walks_the_whole_pool")]
    #[test_case(UNAUTHORIZED, FULL_POOL  ; "unauthorized_walks_the_whole_pool")]
    #[test_case(FORBIDDEN, FULL_POOL     ; "forbidden_walks_the_whole_pool")]
    #[test_case(UNAUTHORIZED, SINGLE_KEY ; "a_lone_key_is_tried_once")]
    fn a_spent_pool_is_walked_once_and_then_gives_up(status: u16, keys: usize) {
        smol::block_on(async {
            let server = PooledServer::new(keys, status, None);

            let result = send_pooled(&server).await;

            assert!(result.is_err(), "no key in the pool is accepted");
            assert_eq!(server.keys_seen().len(), keys, "and no key is tried twice");
        });
    }
}
