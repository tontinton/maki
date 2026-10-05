use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use serde_json::{Value, json};
use tracing::{debug, error, info, warn};

use maki_providers::provider::Provider;
use maki_providers::{
    ContentBlock, ContextGauge, IMAGE_PLACEHOLDER, ImageSource, InputTransformation, Message,
    Model, RequestOptions, Role, StopReason, StreamResponse,
};
use maki_storage::frame::{LiveFacts, PromptFacts};

use super::compaction::{self, CompactReason, CompactSteer, Compacted};
use super::frame::{
    FRAME_REBUILT, FrameFit, RunContextBuilder, context_update, fingerprint, fit_frame,
};
use super::history::{History, sanitize_cancelled_history};
use super::hook::{AgentHooks, AgentSlot};
use super::instructions::{CallInstructions, LoadedInstructions};
use super::streaming::{StreamError, StreamRequest, stream_with_retry};
use super::tool_dispatch::{self, RecentCalls};
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::{McpSession, ToolDeferral};
use crate::permissions::PermissionManager;
use crate::tools::hook::Verdict;
use crate::tools::{Deadline, FileAccess, LocalTools, ToolAudience, ToolContext, ToolFilter};
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, DoneReason, EventSender,
    ExtractedCommand, InputSource, InterruptSource, RunLedger, SessionMailbox, SteerKind,
    TurnCompleteEvent,
};
use maki_config::{ModelPolicy, ToolOutputLines};
use maki_storage::id::SessionRef;

const MAX_REAUTH_ATTEMPTS: u32 = 2;
/// One compaction is the whole remedy for an overflowing prompt, so a second
/// overflow in a row means it did not help and retrying only burns a summary.
const MAX_OVERFLOW_RECOVERIES: u32 = 1;
const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";
/// A model that stalls once often stalls again on the retry, so it gets
/// plenty of chances before the turn ends empty handed.
const MAX_NUDGES: u32 = 20;
/// Counted over non-padding messages.
const RECENT_TOOL_WINDOW: usize = 5;
/// Without this note a cancelled reply replays in history as a finished
/// turn, and a model resuming its own cut-off text can wedge the session
/// (seen with llama.cpp stuck on an unterminated tool call).
const CANCELLED_TEXT_NOTE: &str = "[Response cut off by user cancel]";
const INTERRUPT_NOTE: &str =
    "The user sent a new message while you were working. Address it and continue.";
/// A layer that always answers "continue" would never let the run end, and
/// the user would pay for every turn of it. So `agent.stop` gets this many in
/// a row, and the count starts over whenever the user speaks.
const MAX_STOP_CONTINUATIONS: u32 = 3;
const FIELD_TEXT: &str = "text";
const STOP_FINISHED: &str = "finished";
const STOP_MAX_TOKENS: &str = "max_tokens";
const EMPTIED_MESSAGE: &str = "A plugin emptied the message, so there was nothing to send.";
const NO_FRAME: &str = "no prompt frame to send the request under";
/// The reason the API gives when it drops the old model's thinking after a
/// switch. That is expected and does not mean the prefix moved.
const MODEL_BINDING_MISMATCH: &str = "model_binding_mismatch";
const THINKING_DROPPED: &str =
    "The prompt changed under earlier reasoning, so it was dropped to keep the session going.";

pub fn resolve_compaction_model(
    provider: &Arc<dyn Provider>,
    model: &Model,
    timeouts: maki_providers::Timeouts,
    model_policy: &ModelPolicy,
) -> (Arc<dyn Provider>, Model) {
    if let Some(spec) =
        maki_providers::model_registry::spec_for_tier_any(maki_providers::ModelTier::Compaction)
        && model_policy.allows(&spec)
        && let Ok(mut m) = Model::from_spec(&spec)
        && let Ok(p) = maki_providers::provider::from_model(&mut m, timeouts)
    {
        return (Arc::from(p), m);
    }
    (Arc::clone(provider), model.clone())
}

/// The model and provider a frontend is running. Shared, so a picker can swap
/// it while a run is in flight: [`Agent`] re-reads it every turn.
pub struct ModelSlot {
    pub model: Model,
    pub provider: Arc<dyn Provider>,
}

enum TurnOutcome {
    Continue,
    Done(DoneReason),
}

#[derive(Clone)]
pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub session_id: Option<SessionRef>,
    pub task_id: Option<Arc<str>>,
    pub mailbox: Option<SessionMailbox>,
    pub timeouts: maki_providers::Timeouts,
    pub file_access: Arc<FileAccess>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub subagent_cancels: Arc<CancelMap<String>>,
    /// Subagents inherit this, so a turn's totals cover everything it spawned.
    pub ledger: Arc<RunLedger>,
    pub registry: Arc<crate::tools::ToolRegistry>,
    pub audience: ToolAudience,
    pub model_policy: Arc<ModelPolicy>,
}

pub struct AgentRunParams<'h> {
    pub history: &'h mut History,
    /// Borrowed from the same owner as `history`, since it describes that
    /// transcript. A gauge rebuilt per run would forget every measurement the
    /// session made and fall back to the estimate.
    pub gauge: &'h mut ContextGauge,
    pub event_tx: EventSender,
    /// What the session would start with now. The history's frame is built
    /// from it once, and later only compared against it.
    pub context: RunContextBuilder,
}

pub struct Agent<'h> {
    provider: Arc<dyn Provider>,
    model: Arc<Model>,
    history: &'h mut History,
    gauge: &'h mut ContextGauge,
    event_tx: EventSender,
    context: RunContextBuilder,
    /// What the last context built for this run states. Building one renders
    /// the whole prompt and tools, so it happens at run start, after a
    /// compaction and on a model switch, the only times these facts change
    /// within a run, and not before every request.
    prompt_facts: Option<PromptFacts>,
    mode: AgentMode,
    user_response_rx: Option<Arc<smol::lock::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
    ledger: Arc<RunLedger>,
    num_turns: u32,
    recent_calls: RecentCalls,
    auto_compact: bool,
    loaded_instructions: LoadedInstructions,
    rollback_len: usize,
    carry_from: usize,
    mcp: Option<McpSession>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    reauth_attempts: u32,
    overflow_recoveries: u32,
    permissions: Arc<PermissionManager>,
    opts: RequestOptions,
    session_id: Option<SessionRef>,
    task_id: Option<Arc<str>>,
    mailbox: Option<SessionMailbox>,
    timeouts: maki_providers::Timeouts,
    file_access: Arc<FileAccess>,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    subagent_cancels: Arc<crate::cancel::CancelMap<String>>,
    registry: Arc<crate::tools::ToolRegistry>,
    audience: ToolAudience,
    workflow: bool,
    local_tools: LocalTools,
    model_policy: Arc<ModelPolicy>,
    model_sync: Option<Arc<ArcSwap<ModelSlot>>>,
    stop_continuations: u32,
}

impl<'h> Agent<'h> {
    pub fn new(params: AgentParams, run: AgentRunParams<'h>) -> Self {
        Self {
            provider: params.provider,
            model: Arc::new(params.model),
            config: params.config,
            tool_output_lines: params.tool_output_lines,
            permissions: params.permissions,
            timeouts: params.timeouts,
            history: run.history,
            gauge: run.gauge,
            event_tx: run.event_tx,
            context: run.context,
            prompt_facts: None,
            mode: AgentMode::default(),
            user_response_rx: None,
            interrupt_source: None,
            cancel: CancelToken::none(),
            ledger: params.ledger,
            num_turns: 0,
            recent_calls: RecentCalls::new(),
            auto_compact: compaction::auto_compact_enabled(),
            loaded_instructions: LoadedInstructions::new(),
            rollback_len: 0,
            carry_from: 0,
            mcp: None,
            reauth_attempts: 0,
            overflow_recoveries: 0,
            opts: RequestOptions::default(),
            session_id: params.session_id,
            task_id: params.task_id,
            mailbox: params.mailbox,
            file_access: params.file_access,
            prompt_slots: params.prompt_slots,
            subagent_cancels: params.subagent_cancels,
            registry: params.registry,
            audience: params.audience,
            workflow: false,
            local_tools: LocalTools::default(),
            model_policy: params.model_policy,
            model_sync: None,
            stop_continuations: 0,
        }
    }

    /// Lets the run follow the frontend's model picker. Without it the run
    /// keeps the model it started with.
    pub fn with_model_sync(mut self, slot: Arc<ArcSwap<ModelSlot>>) -> Self {
        self.model_sync = Some(slot);
        self
    }

    pub fn with_mcp(mut self, mcp: Option<McpSession>) -> Self {
        self.mcp = mcp;
        self
    }

    pub fn with_user_response_rx(
        mut self,
        rx: Arc<smol::lock::Mutex<flume::Receiver<String>>>,
    ) -> Self {
        self.user_response_rx = Some(rx);
        self
    }

    pub fn with_interrupt_source(mut self, source: Arc<dyn InterruptSource>) -> Self {
        self.interrupt_source = Some(source);
        self
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_local_tools(mut self, local_tools: LocalTools) -> Self {
        self.local_tools = local_tools;
        self
    }

    pub fn with_loaded_instructions(mut self, loaded: LoadedInstructions) -> Self {
        self.loaded_instructions = loaded;
        self
    }

    /// Cancellation is an ending, not a failure: it comes back as
    /// `Ok(DoneReason::Cancelled)` so callers only report real errors.
    pub async fn run(&mut self, input: AgentInput) -> Result<DoneReason, AgentError> {
        let AgentInput {
            message,
            mode,
            images,
            preamble,
            earlier,
            thinking,
            fast,
            workflow,
            prompt: _,
            source,
        } = input;
        self.rollback_len = self.history.len();
        self.carry_from = self.history.len();
        self.stop_continuations = 0;
        self.mode = mode;
        self.workflow = workflow;
        self.opts = RequestOptions { thinking, fast };
        // Each message of a burst is judged on its own, and one kept message is
        // enough for the run to go on.
        let burst = earlier
            .into_iter()
            .map(|e| (e.message, e.images, e.preamble))
            .chain([(message, images, preamble)]);
        let mut prompt = None;
        for (message, images, preamble) in burst {
            let kept = match self
                .filter_user_message(message, images.len(), source)
                .await
            {
                Err(AgentError::Cancelled) => return self.finish(Err(AgentError::Cancelled)),
                kept => kept?,
            };
            // A burst dropped whole sends nothing, so it must not rebuild the
            // frame either. The next run sees the same change.
            if kept.is_some() && prompt.is_none() {
                self.refresh_frame()?;
            }
            let kept = kept.map(|text| {
                prompt = Some(text.clone());
                Message::user_with_images(text, images)
            });
            self.land_input(preamble, kept)?;
        }
        let Some(message) = prompt else {
            self.emit_done(DoneReason::Dropped)?;
            return Ok(DoneReason::Dropped);
        };

        info!(
            model = %self.model.id,
            mode = ?self.mode,
            message_len = message.len(),
            "agent run started"
        );
        // Subagents are prompted by the machine inside the parent's window;
        // counting them would inflate prompts and busy time.
        let top_level = self.audience.contains(ToolAudience::MAIN);
        if top_level {
            // Tabs and frontends share one process, so attribution follows
            // whichever session actually runs a turn, not whoever last
            // called `set_session_id`.
            if let Some(session) = &self.session_id {
                maki_otel::set_session_id(session.as_str());
            }
            maki_otel::emit::user_prompt(&message);
        }

        if let Some((system, tools)) = self.history.request_prefix() {
            self.gauge
                .seed_if_empty(self.history.as_slice(), system, tools);
        }

        // Every frontend enters here, so busy time is measured here; a turn
        // that failed was still busy.
        let busy_since = Instant::now();
        let result = self.run_loop().await;
        if top_level {
            maki_otel::emit::active_time(busy_since.elapsed());
        }
        self.finish(result)
    }

    fn finish(&mut self, result: Result<DoneReason, AgentError>) -> Result<DoneReason, AgentError> {
        let reason = match result {
            Ok(reason) => reason,
            Err(AgentError::Cancelled) => {
                sanitize_cancelled_history(self.history, self.rollback_len);
                DoneReason::Cancelled
            }
            Err(e) => return Err(e),
        };
        self.emit_done(reason)?;

        Ok(reason)
    }

    /// The preamble lands even when the message was dropped. It holds what the
    /// user already ran (a `!cmd` result), and that is not a layer's to judge.
    /// The mailbox waits for a kept message. Draining it for a dropped one
    /// would bury its notes in a run that ends right away, and eat the wake
    /// they would have caused. Whatever changed goes right before the message,
    /// so the model reads the change first and a rewind takes both.
    fn land_input(
        &mut self,
        preamble: Vec<Message>,
        message: Option<Message>,
    ) -> Result<(), AgentError> {
        for message in preamble {
            self.history.push(message);
        }
        let Some(message) = message else {
            return Ok(());
        };
        if let Some(mailbox) = &self.mailbox {
            for message in mailbox.drain() {
                self.history.push(message);
            }
        }
        self.tell_changes()?;
        if !message.content.is_empty() {
            self.history.push(message);
        }
        Ok(())
    }

    fn turns_exhausted(&self) -> bool {
        self.config
            .max_turns
            .is_some_and(|max| self.num_turns >= max)
    }

    async fn run_loop(&mut self) -> Result<DoneReason, AgentError> {
        loop {
            // Ahead of `try_auto_compact`: a switch landing after the check
            // would compact against the old window and then overflow the new
            // one.
            self.sync_model();
            if self.turns_exhausted() {
                return Ok(DoneReason::MaxTurns);
            }
            self.try_auto_compact().await?;
            match self.turn().await? {
                TurnOutcome::Continue => {}
                TurnOutcome::Done(reason) => return Ok(reason),
            }
        }
    }

    /// The frame lives on while its fingerprint matches. Other tools (a plugin
    /// reload, a model with other capabilities, the workflow toggle) or new
    /// host prompt text can't be told in a message, so they start a new frame,
    /// and the user hears that the cache is gone.
    fn refresh_frame(&mut self) -> Result<(), AgentError> {
        let candidate = (self.context)(&self.model, self.workflow);
        self.prompt_facts.clone_from(&candidate.facts);
        if fit_frame(self.history, candidate, self.mcp.as_ref(), &self.model) == FrameFit::Rebuilt {
            info!(session = ?self.session_id, model = %self.model.id, "frame fingerprint changed, rebuilt the frame");
            self.event_tx.send(AgentEvent::Notice {
                text: FRAME_REBUILT.into(),
            })?;
        }
        Ok(())
    }

    /// Appends an update when the facts differ from what the model last
    /// heard. Every fact is compared every time, so no path that changes one
    /// (a run start, an interrupt switching modes, a model switch, a
    /// compaction) has to remember to tell it.
    fn tell_changes(&mut self) -> Result<(), AgentError> {
        // Plan mode is in no rendered prompt, so only the agent knows it.
        let now = LiveFacts {
            plan: self.mode.plan_path().map(Path::to_path_buf),
            prompt: self.prompt_facts.clone(),
        };
        let Some(update) = self
            .history
            .told()
            .and_then(|told| context_update(&told, &now))
        else {
            return Ok(());
        };
        if let Some(summary) = &update.display_text {
            self.event_tx.send(AgentEvent::Notice {
                text: summary.clone(),
            })?;
        }
        self.history.push(update);
        Ok(())
    }

    /// Picks up a model chosen while the run was working. The prompt stays,
    /// and the next request tells the model about the switch.
    ///
    /// Three kinds of switch wait for the next run. Another provider shapes
    /// history its own way (signed thinking blocks, reasoning fields). Turning
    /// thinking on mid tool loop leaves the last assistant turn without the
    /// thinking block Anthropic then demands. And a model with other tools
    /// (or without tool search) needs a new frame, which only a run start may
    /// build.
    fn sync_model(&mut self) {
        let Some(slot) = &self.model_sync else {
            return;
        };
        let current = slot.load_full();
        if current.model.spec() == self.model.spec()
            || current.model.provider != self.model.provider
            || current.model.supports_thinking() != self.model.supports_thinking()
        {
            return;
        }
        let candidate = (self.context)(&current.model, self.workflow);
        let fingerprint = fingerprint(&candidate, self.mcp.as_ref(), &current.model);
        if self
            .history
            .frame()
            .is_none_or(|frame| frame.fingerprint != fingerprint)
        {
            debug!(model = %current.model.id, "model switch waits for the next run, its frame differs");
            return;
        }
        info!(model = %current.model.id, "adopted model switched mid-run");
        self.prompt_facts = candidate.facts;
        self.provider = Arc::clone(&current.provider);
        self.model = Arc::new(current.model.clone());
    }

    /// Right before every request. A compaction drops the frame, so it may
    /// have to be built again, and MCP servers that connected since the last
    /// request join here too.
    fn prepare_request(&mut self) -> Result<(), AgentError> {
        if self.history.live_frame().is_none() {
            self.refresh_frame()?;
        }
        if let Some(mcp) = &self.mcp {
            self.history
                .append_late_tools(mcp, ToolDeferral::for_model(&self.model));
        }
        // After a cut-off answer the request continues it, and an update
        // there would get a reply instead. The next request tells it.
        let continues_answer = self
            .history
            .as_slice()
            .last()
            .is_some_and(|m| matches!(m.role, Role::Assistant));
        if !continues_answer {
            self.tell_changes()?;
        }
        Ok(())
    }

    async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        self.prepare_request()?;
        let Some((system, tools)) = self.history.request_prefix() else {
            return Err(no_frame());
        };
        let response = match stream_with_retry(
            StreamRequest {
                provider: &*self.provider,
                model: &self.model,
                messages: self.history.as_slice(),
                system,
                tools,
                opts: self.opts,
                output_budget: self.config.max_turn_output,
                session_id: self.session_id.as_ref(),
                retry: self.timeouts.retry,
            },
            Some(self.gauge),
            &self.event_tx,
            &self.cancel,
        )
        .await
        {
            Ok(r) => {
                self.reauth_attempts = 0;
                self.overflow_recoveries = 0;
                r
            }
            Err(StreamError::Cancelled { streamed }) => {
                let streamed = streamed.trim_end();
                if !streamed.is_empty() {
                    self.history.push(Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: format!("{streamed}\n\n{CANCELLED_TEXT_NOTE}"),
                        }],
                        ..Default::default()
                    });
                }
                return Err(AgentError::Cancelled);
            }
            Err(StreamError::Other(e)) if e.is_auth_error() => {
                return self.wait_for_reauth(e).await;
            }
            Err(StreamError::Other(e)) if e.is_context_overflow() => {
                return self.recover_from_overflow(e).await;
            }
            Err(StreamError::Other(e)) if e.is_thinking_unbound() => {
                return self.recover_unbound_thinking(e);
            }
            Err(StreamError::Other(e)) => {
                error!(error = %e, model = %self.model.id, self.num_turns, "stream_message failed");
                return Err(e);
            }
        };
        self.num_turns += 1;
        self.note_input_transformations(&response.input_transformations);

        let has_tools = response.message.has_tool_calls();
        let stop_reason = response.stop_reason;
        info!(
            input_tokens = response.usage.input,
            output_tokens = response.usage.output,
            cache_creation = response.usage.cache_creation,
            cache_read = response.usage.cache_read,
            has_tools,
            self.num_turns,
            model = %self.model.id,
            stop_reason = stop_reason.map_or("none", Into::into),
            "API response received"
        );

        // The gauge already took the provider's own count inside the stream.
        self.emit_turn_complete(&response)?;

        if has_tools {
            let history_len_before = self.history.len();
            self.process_tool_calls(response).await?;
            self.gauge
                .append(&self.history.as_slice()[history_len_before..]);
        } else {
            if response.message.first_text_content().is_some() {
                self.history.push(response.message);
            } else if self.recover_stalled_turn()? {
                return Ok(TurnOutcome::Continue);
            }

            if stop_reason == Some(StopReason::MaxTokens)
                && self.num_turns <= self.config.max_continuation_turns
            {
                warn!(
                    self.num_turns,
                    "response truncated (max_tokens), re-prompting"
                );
                return Ok(TurnOutcome::Continue);
            }
        }

        // Everything the turn appended is answered, so a compaction from here
        // on may summarize it. Input arriving after this point may not.
        self.carry_from = self.history.len();

        if self.handle_queued_command().await? {
            return Ok(TurnOutcome::Continue);
        }

        if has_tools {
            return Ok(TurnOutcome::Continue);
        }
        let reason = stop_reason.into();
        if self.keep_going(reason).await? {
            return Ok(TurnOutcome::Continue);
        }
        Ok(TurnOutcome::Done(reason))
    }

    /// A thinking block the model can no longer see, or one that failed the
    /// prefix check and was let through on an account that does not enforce
    /// it. After a model switch that is normal. Anything else means the
    /// prefix moved, which an enforcing account answers with a 400.
    fn note_input_transformations(&self, transformations: &[InputTransformation]) {
        for t in transformations {
            if t.reason == MODEL_BINDING_MISMATCH {
                info!(session = ?self.session_id, model = %self.model.id, kind = %t.kind, path = %t.path, reason = %t.reason, "thinking from another model not carried over");
            } else {
                warn!(session = ?self.session_id, model = %self.model.id, kind = %t.kind, path = %t.path, reason = %t.reason, flagged = transformations.len(), "thinking block failed the prefix check");
            }
        }
    }

    /// The session's own model and gauge, even when a compaction summarizes
    /// with another model, because that is the run the layers are steering.
    fn hooks(&self) -> AgentHooks<'_> {
        AgentHooks {
            registry: &self.registry,
            session_id: self.session_id.as_ref(),
            task_id: self.task_id.as_deref(),
            model: &self.model,
            cancel: &self.cancel,
            context_size: self.gauge.size(),
        }
    }

    /// `None` means a layer dropped the message. A rewrite is what history
    /// keeps, and the frontend hears about it so the transcript can show the
    /// two differ. A rewrite down to nothing counts as a drop, or the model
    /// would be asked to answer a transcript with nothing new in it. Blank
    /// text comes back empty, since providers refuse a whitespace-only block.
    async fn filter_user_message(
        &self,
        message: String,
        images: usize,
        source: InputSource,
    ) -> Result<Option<String>, AgentError> {
        let blank = |text: &str| text.trim().is_empty();
        if blank(&message) && images == 0 {
            return Ok(Some(String::new()));
        }
        let hooks = self.hooks();
        let verdict = hooks
            .fire(
                AgentSlot::UserMessage,
                || json!({ FIELD_TEXT: message, "images": images, "source": source.as_str() }),
            )
            .await;
        // A layer that redacts must not be skipped by pressing Esc on it.
        if self.cancel.is_cancelled() && hooks.wraps(AgentSlot::UserMessage) {
            return Err(AgentError::Cancelled);
        }
        let dropped = |reason: String| {
            info!(source = source.as_str(), %reason, "agent.user_message dropped the message");
            self.steer(SteerKind::MessageDropped, reason).map(|()| None)
        };
        let text = match verdict {
            Verdict::Replaced(value) => match value.get(FIELD_TEXT).and_then(Value::as_str) {
                Some(text) if blank(text) && images == 0 => {
                    return dropped(EMPTIED_MESSAGE.into());
                }
                Some(text) if text != message => {
                    info!(
                        source = source.as_str(),
                        "agent.user_message rewrote the message"
                    );
                    self.steer(SteerKind::MessageRewritten, text.to_owned())?;
                    text.to_owned()
                }
                _ => message,
            },
            Verdict::Denied(reason) => return dropped(reason),
            Verdict::Unchanged | Verdict::Ask { .. } => message,
        };
        Ok(Some(if blank(&text) { String::new() } else { text }))
    }

    fn steer(&self, kind: SteerKind, text: String) -> Result<(), AgentError> {
        self.event_tx.send(AgentEvent::Steered { kind, text })
    }

    /// True when a layer kept the run going, with its message now last in
    /// history. A run out of turns is not asked, because its continuation
    /// would just sit there unanswered.
    async fn keep_going(&mut self, reason: DoneReason) -> Result<bool, AgentError> {
        if self.stop_continuations >= MAX_STOP_CONTINUATIONS || self.turns_exhausted() {
            return Ok(false);
        }
        let reason = match reason {
            DoneReason::MaxTokens => STOP_MAX_TOKENS,
            _ => STOP_FINISHED,
        };
        let last_message = self
            .history
            .as_slice()
            .last()
            .filter(|m| matches!(m.role, Role::Assistant))
            .and_then(Message::first_text_content)
            .unwrap_or_default();
        let verdict = self
            .hooks()
            .fire(AgentSlot::Stop, || {
                json!({
                    "reason": reason,
                    "last_message": last_message,
                    "num_turns": self.num_turns,
                })
            })
            .await;
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let Verdict::Replaced(value) = verdict else {
            return Ok(false);
        };
        let Some(message) = compaction::continue_text(&value) else {
            return Ok(false);
        };
        self.stop_continuations += 1;
        info!(
            continuations = self.stop_continuations,
            "agent.stop kept the run going"
        );
        self.steer(SteerKind::Continued, message.to_owned())?;
        self.history.push(Message::synthetic(message.to_owned()));
        Ok(true)
    }

    /// The gauge is a chars/4 floor, so a prompt can overflow with the
    /// compaction threshold still unmet. Compaction is the only way out and it
    /// is exactly what the gauge would have asked for, so run it and retry.
    /// The counter resets on every successful stream, so a second overflow
    /// means compaction did not help and the error is the honest answer.
    async fn recover_from_overflow(&mut self, err: AgentError) -> Result<TurnOutcome, AgentError> {
        if !self.auto_compact || self.overflow_recoveries >= MAX_OVERFLOW_RECOVERIES {
            error!(error = %err, model = %self.model.id, self.num_turns, "stream_message failed");
            return Err(err);
        }
        self.overflow_recoveries += 1;
        warn!(
            context_size = self.gauge.size(),
            "prompt overflowed below the compaction threshold"
        );
        self.compact_now(CompactReason::Overflow).await?;
        Ok(TurnOutcome::Continue)
    }

    /// The account enforces thinking binding and something before a thinking
    /// block moved: a rebuilt frame, an evicted image, a tool that joined in
    /// full. The same body fails the same way every time, so every thinking
    /// block goes, once. That edit is valid on every endpoint and leaves the
    /// prefix append-only from here on. With none left to drop, the error is
    /// the honest answer.
    fn recover_unbound_thinking(&mut self, err: AgentError) -> Result<TurnOutcome, AgentError> {
        let dropped = self.history.strip_thinking();
        if dropped == 0 {
            error!(error = %err, model = %self.model.id, self.num_turns, "stream_message failed");
            return Err(err);
        }
        warn!(session = ?self.session_id, model = %self.model.id, dropped, error = %err, "thinking bound to a moved prefix, dropped it all");
        self.event_tx.send(AgentEvent::Notice {
            text: THINKING_DROPPED.into(),
        })?;
        Ok(TurnOutcome::Continue)
    }

    async fn wait_for_reauth(&mut self, err: AgentError) -> Result<TurnOutcome, AgentError> {
        if self.reauth_attempts >= MAX_REAUTH_ATTEMPTS {
            error!(error = %err, attempts = self.reauth_attempts, "max re-auth attempts reached");
            return Err(err);
        }
        let Some(rx) = &self.user_response_rx else {
            error!(error = %err, model = %self.model.id, self.num_turns, "stream_message failed");
            return Err(err);
        };
        self.reauth_attempts += 1;
        warn!(error = %err, attempt = self.reauth_attempts, "auth error, waiting for re-authentication");
        self.event_tx.send(AgentEvent::AuthRequired)?;
        let rx = rx.lock().await;
        match futures_lite::future::race(rx.recv_async(), async {
            self.cancel.cancelled().await;
            Err(flume::RecvError::Disconnected)
        })
        .await
        {
            Ok(_) => {
                self.provider.refresh_auth().await?;
                Ok(TurnOutcome::Continue)
            }
            Err(_) => Err(AgentError::Cancelled),
        }
    }

    fn emit_turn_complete(&self, response: &StreamResponse) -> Result<(), AgentError> {
        let cost = self.model.billed_cost(&response.usage, self.opts.fast);
        // The ledger banks the un-subsidised list price for every model, not
        // just subsidised ones, so `Done.list_cost` is a real total on a
        // metered run instead of the auto-compaction turns alone. The two
        // agree on a subsidised model, which is why the event can narrow to
        // the reference figure the UI shows beside its `$0` bill.
        self.ledger.add(
            response.usage,
            cost,
            self.model.list_cost(&response.usage, self.opts.fast),
        );
        self.event_tx
            .send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: response.message.clone(),
                usage: response.usage,
                model: self.model.id.clone(),
                cost,
                subsidised_list_cost: self
                    .model
                    .subsidised_list_cost(&response.usage, self.opts.fast),
                context_size: Some(self.gauge.size()),
                context_window: self.model.context_window,
            })))
    }

    fn emit_done(&self, reason: DoneReason) -> Result<(), AgentError> {
        let totals = self.ledger.totals();
        info!(
            self.num_turns,
            total_input = totals.usage.input,
            total_output = totals.usage.output,
            %reason,
            "agent run completed"
        );
        self.event_tx.send(AgentEvent::Done {
            usage: totals.usage,
            cost: totals.cost,
            list_cost: totals.list_cost,
            context_size: self.gauge.size(),
            context_window: self.model.context_window,
            num_turns: self.num_turns,
            reason,
        })
    }

    /// The turn came back without text, so [`Message::empty_marker`] takes its
    /// place in history. Returns true when the model was nudged to try again.
    fn recover_stalled_turn(&mut self) -> Result<bool, AgentError> {
        let nudges = self.history.recent_nudges();
        let nudge = nudges < MAX_NUDGES && self.history.has_recent_tool_results(RECENT_TOOL_WINDOW);
        self.history.push(Message::empty_marker());
        if !nudge {
            return Ok(false);
        }

        warn!(
            nudges = nudges + 1,
            "empty response after tool calls, nudging model to continue"
        );
        self.event_tx.send(AgentEvent::Nudge)?;
        self.history.push(Message::synthetic(NUDGE_PROMPT.into()));
        Ok(true)
    }

    /// Fails closed: the filter is the frame's, and without one no tool may
    /// run under some other set.
    async fn process_tool_calls(&mut self, response: StreamResponse) -> Result<(), AgentError> {
        let filter = self
            .history
            .live_frame()
            .map(|live| Arc::clone(live.tools.filter()))
            .ok_or_else(no_frame)?;
        let ctx = self.tool_context(filter);
        tool_dispatch::process_tool_calls(
            response,
            &mut self.recent_calls,
            self.history,
            &self.event_tx,
            &ctx,
        )
        .await
    }

    fn tool_context(&self, tool_filter: Arc<ToolFilter>) -> ToolContext {
        ToolContext {
            provider: Arc::clone(&self.provider),
            model: Arc::clone(&self.model),
            event_tx: self.event_tx.clone(),
            mode: self.mode.clone(),
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            tool_use_id: None,
            user_response_rx: self.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            call_instructions: CallInstructions::default(),
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
            deadline: Deadline::None,
            config: self.config.clone(),
            tool_filter,
            tool_output_lines: self.tool_output_lines,
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            file_access: Arc::clone(&self.file_access),
            prompt_slots: Arc::clone(&self.prompt_slots),
            opts: self.opts,
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            ledger: Arc::clone(&self.ledger),
            registry: Arc::clone(&self.registry),
            workflow: self.workflow,
            audience: self.audience,
            local_tools: Arc::clone(&self.local_tools),
            live_sink: None,
            model_policy: Arc::clone(&self.model_policy),
        }
    }

    async fn try_auto_compact(&mut self) -> Result<(), AgentError> {
        let context_size = self.gauge.size();
        if !self.auto_compact || !compaction::is_overflow(context_size, &self.model, &self.config) {
            return Ok(());
        }
        info!(context_size, "auto-compacting");
        self.compact_now(CompactReason::Auto).await
    }

    /// A layer may skip an automatic compaction to wait for a better moment,
    /// but only until the provider itself refuses the prompt.
    async fn compact_now(&mut self, reason: CompactReason) -> Result<(), AgentError> {
        let Some(steer) =
            compaction::steer_compaction(&self.hooks(), &self.config, reason, None).await
        else {
            return Ok(());
        };
        self.event_tx.send(AgentEvent::AutoCompacting {
            context_size: self.gauge.size(),
            context_window: self.model.context_window,
        })?;
        self.do_compact(steer).await
    }

    async fn do_compact(&mut self, steer: CompactSteer) -> Result<(), AgentError> {
        // Compaction replaces the whole transcript, so input no turn has
        // answered yet would be summarized away before the model ever saw it,
        // images and all. `carry_from` is where that input starts: the run's
        // own prompt, plus anything queued in since the last turn ended.
        let carry_len = self.history.len().saturating_sub(self.carry_from);
        let carried = self
            .compact_and_bill(steer.instructions.as_deref(), carry_len)
            .await?;
        // An unanswered prompt says what to do next better than the generic
        // nudge, so it stands in for it. A layer's own words still go in.
        if carried == 0 {
            self.history
                .push(Message::synthetic(compaction::continue_message(
                    &self.config,
                    steer.continue_text.as_deref(),
                )));
        } else if let Some(text) = steer.continue_text {
            self.history.push(Message::synthetic(text));
        }
        Ok(())
    }

    async fn compact_and_bill(
        &mut self,
        instructions: Option<&str>,
        carry_len: usize,
    ) -> Result<usize, AgentError> {
        let context_size_before = self.gauge.size();
        let (compact_provider, compact_model) = resolve_compaction_model(
            &self.provider,
            &self.model,
            self.timeouts,
            &self.model_policy,
        );
        // Built from the fields, not `self.hooks()`, which would borrow all of
        // `self` while `history` is lent out mutably.
        let hooks = AgentHooks {
            registry: &self.registry,
            session_id: self.session_id.as_ref(),
            task_id: self.task_id.as_deref(),
            model: &self.model,
            cancel: &self.cancel,
            context_size: context_size_before,
        };
        let Compacted {
            usage: compaction_usage,
            summary,
            carried,
        } = compaction::compact_history(
            &*compact_provider,
            &compact_model,
            self.history,
            &self.event_tx,
            &hooks,
            &self.config,
            instructions,
            carry_len,
            self.timeouts.retry,
        )
        .await?;
        // The summariser can be a different model, so price this with
        // `compact_model` and not `self.model`. `list_cost` gates `fast`
        // against whichever one it gets, and is the un-subsidised price the
        // ledger wants either way.
        let compact_cost = compact_model.billed_cost(&compaction_usage, self.opts.fast);
        let compact_list_cost = compact_model.list_cost(&compaction_usage, self.opts.fast);
        self.ledger
            .add(compaction_usage, compact_cost, compact_list_cost);
        let carry_from = self.history.len() - carried;
        self.rollback_len = carry_from;
        self.carry_from = carry_from;
        // The old prefix has nothing worth keeping now, so the new prompt picks
        // up every fact told since.
        self.refresh_frame()?;
        // The measurement the gauge holds describes the transcript that was
        // just summarized away, so what is left is all it may count.
        if let Some((system, tools)) = self.history.request_prefix() {
            self.gauge.reset(self.history.as_slice(), system, tools);
        }
        let context_size_after = self.gauge.size();
        self.event_tx.send(AgentEvent::CompactionDone {
            context_size_before,
            context_size_after,
            context_window: self.model.context_window,
            summary,
        })?;
        Ok(carried)
    }

    async fn handle_queued_command(&mut self) -> Result<bool, AgentError> {
        let Some(ref source) = self.interrupt_source else {
            return Ok(false);
        };
        let Some(cmd) = source.poll() else {
            return Ok(false);
        };
        // True only when something new landed for the model. Going on without
        // it would send the finished answer back as the last message.
        match cmd {
            // The burst lands as consecutive user messages, so one request
            // carries all of it.
            ExtractedCommand::Interrupt(inputs) => {
                let mut kept_any = false;
                for input in inputs {
                    self.event_tx.send(AgentEvent::QueueItemConsumed {
                        text: input.message.clone(),
                        images: input.images.clone(),
                    })?;
                    let kept = self
                        .filter_user_message(input.message, input.images.len(), input.source)
                        .await?;
                    let message = kept.map(|text| interrupt_message(text, input.images));
                    if message.is_some() {
                        self.mode = input.mode;
                        // The user spoke, so `agent.stop` gets its full
                        // allowance back.
                        self.stop_continuations = 0;
                        kept_any = true;
                    }
                    self.land_input(input.preamble, message)?;
                }
                Ok(kept_any)
            }
            ExtractedCommand::Compact(instructions) => {
                let Some(steer) = compaction::steer_compaction(
                    &self.hooks(),
                    &self.config,
                    CompactReason::Manual,
                    instructions.as_deref(),
                )
                .await
                else {
                    return Ok(false);
                };
                self.do_compact(steer).await?;
                Ok(true)
            }
        }
    }
}

fn no_frame() -> AgentError {
    AgentError::Config {
        message: NO_FRAME.into(),
    }
}

fn interrupt_message(message: String, images: Vec<ImageSource>) -> Message {
    let wrapped = format!("<user-interrupt>\n{INTERRUPT_NOTE}\n\n{message}\n</user-interrupt>");
    Message {
        display_text: Some(if message.is_empty() && !images.is_empty() {
            IMAGE_PLACEHOLDER.into()
        } else {
            message
        }),
        ..Message::user_with_images(wrapped, images)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use maki_config::ProjectConfig;
    use maki_providers::provider::{BoxFuture, Provider};
    use maki_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, ThinkingSupport, TokenUsage,
    };
    use serde_json::Value;
    use test_case::test_case;

    use super::compaction::FIELD_CONTINUE;
    use super::*;
    use crate::agent::frame::{PLAN_ENDED, RunContext};
    use crate::agent::hook::testing::{Seen, script};
    use crate::mcp::test_support::stub_session;
    use crate::mcp::tool_names;
    use crate::permissions::PermissionManager;
    use crate::tools::RequestTools;
    use crate::{EarlierInput, Envelope};

    const QUEUED_MESSAGES: [&str; 3] = ["first", "second", "third"];
    const RESPONSE_TEXT: &str = "response";
    const ONE_GAUGE_MSG: &str =
        "TurnComplete, Done, and the compaction trigger must read one context gauge";

    struct MockInterruptSource {
        commands: Mutex<VecDeque<ExtractedCommand>>,
    }

    impl MockInterruptSource {
        fn new(commands: Vec<ExtractedCommand>) -> Arc<Self> {
            Arc::new(Self {
                commands: Mutex::new(commands.into()),
            })
        }
    }

    impl InterruptSource for MockInterruptSource {
        fn poll(&self) -> Option<ExtractedCommand> {
            self.commands.lock().unwrap().pop_front()
        }
    }

    /// Messages are kept serialized, because the cache and the thinking
    /// bindings compare bytes, not values.
    struct CapturedRequest {
        model: String,
        system: String,
        tools: Value,
        messages: Vec<Value>,
    }

    /// The contract that keeps the cache and every thinking block valid:
    /// `system` and `tools` only ever grow at the end, and so do messages.
    #[track_caller]
    fn assert_append_only(prev: &CapturedRequest, next: &CapturedRequest) {
        assert_eq!(prev.system, next.system, "system changed");
        let (prev_tools, next_tools) = (
            prev.tools.as_array().unwrap(),
            next.tools.as_array().unwrap(),
        );
        assert!(next_tools.starts_with(prev_tools), "tools were edited");
        assert!(
            next.messages.starts_with(&prev.messages),
            "earlier messages were edited"
        );
    }

    struct MockProvider {
        responses: Mutex<Vec<StreamResponse>>,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Arc::default(),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                self.requests.lock().unwrap().push(CapturedRequest {
                    model: model.id.clone(),
                    system: system.to_owned(),
                    tools: tools.clone(),
                    messages: messages
                        .iter()
                        .map(|m| serde_json::to_value(m).unwrap())
                        .collect(),
                });
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                Ok(responses.remove(0))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// Streams `delta` (if any), fires `cancel_after_delta` (if any),
    /// then fails with `fail_status` or hangs until cancelled.
    #[derive(Default)]
    struct StubStreamProvider {
        delta: Option<&'static str>,
        cancel_after_delta: Mutex<Option<crate::cancel::CancelTrigger>>,
        fail_status: Option<u16>,
    }

    impl Provider for StubStreamProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            ptx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                if let Some(text) = self.delta {
                    ptx.send(ProviderEvent::TextDelta { text: text.into() })
                        .unwrap();
                }
                if let Some(trigger) = self.cancel_after_delta.lock().unwrap().take() {
                    trigger.cancel();
                }
                match self.fail_status {
                    Some(status) => Err(AgentError::api(status, "stub")),
                    None => futures_lite::future::pending().await,
                }
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn default_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn tool_search_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-5").unwrap()
    }

    fn text_response(stop_reason: StopReason) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: RESPONSE_TEXT.into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(stop_reason),
            ..Default::default()
        }
    }

    fn empty_response() -> StreamResponse {
        assistant_response(vec![])
    }

    fn thinking_response() -> StreamResponse {
        assistant_response(vec![ContentBlock::Thinking {
            thinking: "stalled".into(),
            signature: None,
        }])
    }

    fn assistant_response(content: Vec<ContentBlock>) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content,
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
            ..Default::default()
        }
    }

    fn make_agent(
        provider: impl Provider + 'static,
        history: &mut History,
    ) -> (Agent<'_>, flume::Receiver<Envelope>) {
        make_agent_with(Arc::new(provider), default_model(), history)
    }

    /// The leaked gauge outlives the agent that borrows it, so no caller here
    /// has to own one.
    fn make_agent_with(
        provider: Arc<dyn Provider>,
        model: Model,
        history: &mut History,
    ) -> (Agent<'_>, flume::Receiver<Envelope>) {
        let (raw_tx, event_rx) = flume::unbounded();
        let agent = Agent::new(
            AgentParams {
                provider,
                model,
                config: AgentConfig::default(),
                tool_output_lines: ToolOutputLines::default(),
                permissions: Arc::new(PermissionManager::new(
                    maki_config::PermissionsConfig {
                        default: maki_config::DefaultEffect::Allow,
                        rules: vec![],
                        ..Default::default()
                    },
                    std::path::PathBuf::from("/tmp"),
                    ProjectConfig::for_project(Path::new("/tmp")),
                    Arc::default(),
                )),
                session_id: None,
                task_id: None,
                mailbox: None,
                timeouts: maki_providers::Timeouts::default(),
                file_access: FileAccess::fresh(),
                prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
                subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
                ledger: Arc::new(RunLedger::default()),
                registry: Arc::new(crate::tools::ToolRegistry::new()),
                audience: ToolAudience::MAIN,
                model_policy: Arc::new(ModelPolicy::default()),
            },
            AgentRunParams {
                history,
                gauge: Box::leak(Box::default()),
                event_tx: EventSender::new(raw_tx, 0),
                context: Arc::new(|_, _| {
                    RunContext::fixed("system".into(), RequestTools::default())
                }),
            },
        );
        (agent, event_rx)
    }

    fn default_input() -> AgentInput {
        AgentInput {
            message: "hello".into(),
            mode: AgentMode::Build,
            images: Vec::new(),
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking: Default::default(),
            fast: false,
            workflow: false,
            prompt: None,
            source: InputSource::Tui,
        }
    }

    #[test]
    fn run_ingests_preamble_then_mailbox_then_user_message() {
        smol::block_on(async {
            let id = maki_storage::id::MakiId::generate();
            let mailbox = SessionMailbox::register(id);
            SessionMailbox::notify(id, "mailbox".into(), false).unwrap();
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.mailbox = Some(mailbox);
            let mut input = default_input();
            input.preamble = vec![Message::observation("preamble".into())];

            agent.run(input).await.unwrap();
            drop(agent);

            assert_eq!(history.as_slice()[0].user_text(), Some("preamble"));
            assert_eq!(history.as_slice()[1].user_text(), Some("mailbox"));
            assert_eq!(history.as_slice()[2].user_text(), Some("hello"));
        });
    }

    #[test]
    fn queued_input_drains_preamble_and_mailbox() {
        smol::block_on(async {
            let id = maki_storage::id::MakiId::generate();
            let mailbox = SessionMailbox::register(id);
            SessionMailbox::notify(id, "mailbox".into(), false).unwrap();
            let mut input = default_input();
            input.preamble = vec![Message::observation("preamble".into())];
            let source = MockInterruptSource::new(vec![ExtractedCommand::Interrupt(vec![input])]);
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            agent.mailbox = Some(mailbox);
            let mut agent = agent.with_interrupt_source(source);

            assert!(agent.handle_queued_command().await.unwrap());
            drop(agent);

            let text = history
                .as_slice()
                .iter()
                .map(Message::user_text)
                .collect::<Vec<_>>();
            assert_eq!(text, [Some("preamble"), Some("mailbox"), Some("hello")]);
            assert!(history.as_slice()[0].is_observation());
            assert!(history.as_slice()[1].is_observation());
        });
    }

    #[test]
    fn wake_only_run_does_not_insert_an_empty_user_turn() {
        smol::block_on(async {
            let id = maki_storage::id::MakiId::generate();
            let mailbox = SessionMailbox::register(id);
            SessionMailbox::notify(id, "failed".into(), true).unwrap();
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.mailbox = Some(mailbox);
            let mut input = default_input();
            input.message.clear();

            agent.run(input).await.unwrap();
            drop(agent);

            assert_eq!(history.as_slice().len(), 2);
            assert!(history.as_slice()[0].is_observation());
            assert!(matches!(history.as_slice()[1].role, Role::Assistant));
        });
    }

    fn drain_events(rx: &flume::Receiver<Envelope>) -> Vec<Envelope> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    async fn run_agent(provider: MockProvider, max_turns: Option<u32>) -> (u32, DoneReason) {
        let mut history = History::new(Vec::new());
        let (mut agent, event_rx) = make_agent(provider, &mut history);
        agent.config.max_turns = max_turns;
        let _ = agent.run(default_input()).await;
        drain_events(&event_rx)
            .into_iter()
            .find_map(|e| match e.event {
                AgentEvent::Done {
                    num_turns, reason, ..
                } => Some((num_turns, reason)),
                _ => None,
            })
            .expect("expected Done event")
    }

    fn has_event(events: &[Envelope], predicate: impl Fn(&AgentEvent) -> bool) -> bool {
        events.iter().any(|e| predicate(&e.event))
    }

    fn has_interrupt_in_history(history: &[Message]) -> bool {
        history.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.contains("<user-interrupt>")),
            )
        })
    }

    fn tool_call_response(tool_name: &str, tool_id: &str) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    tool_id,
                    tool_name,
                    serde_json::json!({"pattern": "*.nonexistent_test_xyz", "path": "/tmp"}),
                )],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        }
    }

    fn tool_use_response(tool_name: &str, input: Value) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", tool_name, input)],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        }
    }

    fn search_turn(model: Model) -> (Vec<Value>, ContentBlock) {
        smol::block_on(async {
            let provider = MockProvider::new(vec![
                tool_use_response(
                    crate::mcp::TOOL_SEARCH_TOOL_NAME,
                    serde_json::json!({"query": "fetch issue"}),
                ),
                text_response(StopReason::EndTurn),
            ]);
            let captured = Arc::clone(&provider.requests);
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent_with(Arc::new(provider), model, &mut history);
            let mut agent = agent.with_mcp(Some(crate::mcp::test_support::stub_session(&[(
                "srv.fetch_issue",
                "Fetch a GitHub issue",
            )])));
            agent.run(default_input()).await.unwrap();

            let tools = captured
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.tools.clone())
                .collect();
            let result = history
                .as_slice()
                .iter()
                .flat_map(|m| &m.content)
                .find(|b| matches!(b, ContentBlock::ToolResult { .. }))
                .cloned()
                .unwrap();
            (tools, result)
        })
    }

    #[test]
    fn mcp_definitions_refresh_per_request() {
        let (tools, result) = search_turn(Model::from_spec("openai/gpt-5").unwrap());
        assert_eq!(tools.len(), 2);
        let first = tool_names(&tools[0]);
        assert!(first.contains(&crate::mcp::TOOL_SEARCH_TOOL_NAME));
        assert!(!first.contains(&"srv__fetch_issue"));
        assert!(tool_names(&tools[1]).contains(&"srv__fetch_issue"));
        assert!(
            matches!(&result, ContentBlock::ToolResult { loaded_tools, .. } if loaded_tools == &["srv__fetch_issue"]),
            "got: {result:?}"
        );
    }

    #[test]
    fn native_deferral_keeps_tools_array_stable_across_a_load() {
        let (tools, result) = search_turn(tool_search_model());
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0], tools[1]);
        let deferred = tools[0]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "srv__fetch_issue")
            .expect("deferred definition ships in the array");
        assert!(maki_providers::is_deferred_tool(deferred));
        assert!(
            matches!(&result, ContentBlock::ToolResult { loaded_tools, .. } if loaded_tools == &["srv__fetch_issue"]),
            "got: {result:?}"
        );
    }

    fn small_context_model(context_window: u32, max_output_tokens: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model.max_output_tokens = Some(max_output_tokens);
        model
    }

    #[track_caller]
    fn assert_ends_with_cancel_marker(history: &History) {
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert!(
            matches!(&last.content[0], ContentBlock::Text { text } if text == "[Cancelled by user]")
        );
    }

    /// A truncated answer buys another turn, but only until one of the two
    /// budgets runs out: the continuation limit or the caller's `max_turns`.
    #[test_case(&[StopReason::EndTurn], None, 1, DoneReason::EndTurn ; "end_turn_completes")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn], None, 2, DoneReason::EndTurn ; "max_tokens_continues")]
    #[test_case(&[StopReason::MaxTokens; 4], None, 4, DoneReason::MaxTokens ; "max_tokens_gives_up_after_limit")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn], Some(1), 1, DoneReason::MaxTurns ; "turn_budget_exhausted")]
    fn turn_counting(
        stops: &[StopReason],
        max_turns: Option<u32>,
        expected_turns: u32,
        expected_reason: DoneReason,
    ) {
        smol::block_on(async {
            let responses: Vec<_> = stops.iter().map(|s| text_response(*s)).collect();
            let provider = MockProvider::new(responses);
            let (turns, reason) = run_agent(provider, max_turns).await;
            assert_eq!(turns, expected_turns);
            assert_eq!(reason, expected_reason);
        });
    }

    #[test_case(Some(true),  true,  true  ; "after_tool_use_turn")]
    #[test_case(Some(false), true,  true  ; "after_text_only_turn")]
    #[test_case(None,        false, false ; "channel_empty")]
    fn interrupt_handling(queued: Option<bool>, expect_consumed: bool, expect_injected: bool) {
        smol::block_on(async {
            let source = if queued.is_some() {
                Some(MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                    vec![default_input()],
                )]))
            } else {
                None
            };

            let tool_use = queued.unwrap_or(true);
            let responses = if tool_use {
                vec![
                    tool_call_response("glob", "t1"),
                    text_response(StopReason::EndTurn),
                ]
            } else {
                vec![
                    text_response(StopReason::EndTurn),
                    text_response(StopReason::EndTurn),
                ]
            };

            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            if let Some(s) = source {
                agent = agent.with_interrupt_source(s);
            }
            let _ = agent.run(default_input()).await;
            let events = drain_events(&event_rx);

            assert_eq!(
                has_event(&events, |e| matches!(
                    e,
                    AgentEvent::QueueItemConsumed { .. }
                )),
                expect_consumed,
            );
            assert_eq!(
                has_interrupt_in_history(history.as_slice()),
                expect_injected
            );
        });
    }

    /// Stands in for a user picking another model while the run works. The
    /// agent only reads the slot between turns, so storing on every poll still
    /// leaves the opening request on the model the run started with.
    struct ModelPicker {
        slot: Arc<ArcSwap<ModelSlot>>,
        provider: Arc<dyn Provider>,
        model: Model,
    }

    impl InterruptSource for ModelPicker {
        fn poll(&self) -> Option<ExtractedCommand> {
            self.slot.store(Arc::new(ModelSlot {
                model: self.model.clone(),
                provider: Arc::clone(&self.provider),
            }));
            None
        }
    }

    const ADOPTED_MSG: &str =
        "the next request must go out on the new model, and the model told so";
    const HELD_MSG: &str = "this switch must wait for the next run, nothing may move";

    /// A picker can swap the model while a run sits between turns. The next
    /// request goes out on it under the same frame, and an update tells the
    /// model. Another provider, a thinking change or other tools wait for the
    /// next run (see `sync_model` for why).
    #[test_case("anthropic", ThinkingSupport::No, true, true ; "same_provider_thinking_and_tools")]
    #[test_case("anthropic", ThinkingSupport::No, false, false ; "tools_changed")]
    #[test_case("anthropic", ThinkingSupport::Yes, true, false ; "thinking_support_changed")]
    #[test_case("openai", ThinkingSupport::No, true, false ; "provider_changed")]
    fn model_switched_between_turns(
        switched_provider: &str,
        switched_thinking: ThinkingSupport,
        same_tools: bool,
        adopted: bool,
    ) {
        smol::block_on(async {
            let mock = MockProvider::new(vec![
                tool_call_response("glob", "t1"),
                text_response(StopReason::EndTurn),
            ]);
            let requests = Arc::clone(&mock.requests);
            let provider: Arc<dyn Provider> = Arc::new(mock);
            let start = Model {
                thinking_override: Some(ThinkingSupport::No),
                ..default_model()
            };
            let switched = Model {
                id: "claude-opus-4-1-20250805".into(),
                provider: switched_provider.into(),
                thinking_override: Some(switched_thinking),
                ..start.clone()
            };
            let slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
                model: start.clone(),
                provider: Arc::clone(&provider),
            }));
            let picker = Arc::new(ModelPicker {
                slot: Arc::clone(&slot),
                provider: Arc::clone(&provider),
                model: switched.clone(),
            });
            let build = move |model: &Model, _: bool| {
                let tool = if same_tools {
                    "glob".to_owned()
                } else {
                    format!("tool_for_{}", model.id)
                };
                RunContext {
                    system: "system".into(),
                    tools: RequestTools::assembled(
                        serde_json::json!([{ "name": tool }]),
                        &AgentConfig::default(),
                        model,
                    ),
                    facts: Some(PromptFacts {
                        model: model.spec(),
                        ..Default::default()
                    }),
                    authored: String::new(),
                }
            };

            let mut history = History::new(Vec::new());
            let (agent, _event_rx) =
                make_agent_with(Arc::clone(&provider), start.clone(), &mut history);
            let mut agent = agent.with_model_sync(slot).with_interrupt_source(picker);
            agent.context = Arc::new(build);
            agent.run(default_input()).await.unwrap();
            drop(agent);

            let requests = requests.lock().unwrap();
            let expected = if adopted { &switched } else { &start };
            let msg = if adopted { ADOPTED_MSG } else { HELD_MSG };
            assert_eq!(requests[0].model, start.id);
            assert_eq!(requests[1].model, expected.id, "{msg}");
            assert_append_only(&requests[0], &requests[1]);
            let told = updates(&history)
                .iter()
                .any(|update| update.contains(&switched.spec()));
            assert_eq!(told, adopted, "{msg}");
        });
    }

    const NEXT_DATE: &str = "2026-10-03";
    const MEMORY_HINT: &str = "memory/after_instructions";
    const PLAN_FILE: &str = "/plans/p.md";
    const SWITCHED_MODEL: &str = "claude-opus-4-1-20250805";
    const PLAN_TOLD_ONCE_MSG: &str =
        "plan mode is in no rendered prompt, so after a compaction it must be told exactly once";

    /// What a frontend would render, under the test's control. The prompt
    /// spells out every fact, so a rebuilt prompt can never pass for the old one.
    #[derive(Clone, Default)]
    struct World {
        facts: Arc<Mutex<PromptFacts>>,
        tool: Arc<Mutex<&'static str>>,
        authored: Arc<Mutex<&'static str>>,
        /// The host wrote the prompt, so it states no facts maki knows of.
        host_written: Arc<Mutex<bool>>,
    }

    impl World {
        fn change(&self, f: impl FnOnce(&mut PromptFacts)) {
            f(&mut self.facts.lock().unwrap());
        }

        fn builder(&self) -> RunContextBuilder {
            let world = self.clone();
            Arc::new(move |model, _| {
                let facts = PromptFacts {
                    model: model.spec(),
                    ..world.facts.lock().unwrap().clone()
                };
                RunContext {
                    system: format!("system {facts:?}"),
                    tools: RequestTools::assembled(
                        serde_json::json!([{ "name": *world.tool.lock().unwrap() }]),
                        &AgentConfig::default(),
                        model,
                    ),
                    facts: (!*world.host_written.lock().unwrap()).then_some(facts),
                    authored: world.authored.lock().unwrap().to_string(),
                }
            })
        }
    }

    /// One session over many runs and restarts, with every request it sent.
    struct Harness {
        world: World,
        model: Model,
        history: History,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    impl Harness {
        fn new(prior: Vec<Message>) -> Self {
            Self {
                world: World::default(),
                model: default_model(),
                history: History::new(prior),
                requests: Arc::default(),
            }
        }

        fn run(
            &mut self,
            input: AgentInput,
            responses: Vec<StreamResponse>,
            configure: impl FnOnce(&mut Agent<'_>),
        ) -> (DoneReason, Vec<Envelope>) {
            smol::block_on(async {
                let mut mock = MockProvider::new(responses);
                mock.requests = Arc::clone(&self.requests);
                let (mut agent, event_rx) =
                    make_agent_with(Arc::new(mock), self.model.clone(), &mut self.history);
                agent.context = self.world.builder();
                configure(&mut agent);
                let reason = agent.run(input).await.unwrap();
                drop(agent);
                (reason, drain_events(&event_rx))
            })
        }

        fn run_in(&mut self, mode: AgentMode) -> Vec<Envelope> {
            let input = AgentInput {
                mode,
                ..default_input()
            };
            self.run(input, vec![text_response(StopReason::EndTurn)], |_| {})
                .1
        }

        /// A new process loading the session, cut to `kept` messages.
        fn restart(&mut self, kept: usize) {
            let mut messages = self.history.as_slice().to_vec();
            messages.truncate(kept);
            self.history = History::restored(messages).with_frame(self.history.frame().cloned());
        }

        fn assert_append_only(&self) {
            for pair in self.requests.lock().unwrap().windows(2) {
                assert_append_only(&pair[0], &pair[1]);
            }
        }
    }

    fn updates(history: &History) -> Vec<&str> {
        history
            .as_slice()
            .iter()
            .filter(|m| m.is_context_update())
            .filter_map(Message::first_text_content)
            .collect()
    }

    fn with_compact(agent: &mut Agent<'_>) {
        let compact = vec![ExtractedCommand::Compact(None)];
        agent.interrupt_source = Some(MockInterruptSource::new(compact));
    }

    /// Saving a memory used to reorder a hint line in the prompt and throw the
    /// whole cache away. Now that change, like a model switch, arrives as a
    /// message at the end.
    #[test_case(|h: &mut Harness| h.world.change(|f| { f.hints.insert(MEMORY_HINT.into(), "tags: b, a".into()); }), MEMORY_HINT ; "memory_write")]
    #[test_case(|h: &mut Harness| h.model.id = SWITCHED_MODEL.into(), SWITCHED_MODEL ; "model_switch")]
    fn change_between_runs_is_appended(change: fn(&mut Harness), says: &str) {
        let mut h = Harness::new(Vec::new());
        h.run_in(AgentMode::Build);
        change(&mut h);
        h.run_in(AgentMode::Build);

        h.assert_append_only();
        let updates = updates(&h.history);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].contains(says));
    }

    #[test]
    fn plan_mode_round_trip_is_appended() {
        let mut h = Harness::new(Vec::new());
        let plan = AgentMode::Plan(PLAN_FILE.into());
        for mode in [plan.clone(), AgentMode::Build, plan] {
            h.run_in(mode);
        }

        h.assert_append_only();
        let updates = updates(&h.history);
        assert_eq!(updates.len(), 3);
        assert!(updates[0].contains(PLAN_FILE));
        assert!(updates[1].contains(PLAN_ENDED));
        assert!(updates[2].contains(PLAN_FILE));
    }

    /// Neither fits in an update. Other tools need another array, and a prompt
    /// the host wrote itself (an sdk `--system-prompt`) is not a fact.
    #[test_case(|w: &World| *w.tool.lock().unwrap() = "reloaded" ; "tools")]
    #[test_case(|w: &World| *w.authored.lock().unwrap() = "custom prompt" ; "authored_prompt")]
    fn fingerprint_change_starts_a_new_frame_with_a_notice(change: fn(&World)) {
        let mut h = Harness::new(Vec::new());
        h.run_in(AgentMode::Build);
        let first_frame = h.history.frame().cloned();
        change(&h.world);
        let events = h.run_in(AgentMode::Build);

        assert_ne!(h.history.frame().cloned(), first_frame);
        assert!(events.iter().any(|e| matches!(
            &e.event,
            AgentEvent::Notice { text } if text == FRAME_REBUILT
        )));
    }

    /// Swapping maki's prompt for one the host wrote, or back, rebuilds the
    /// frame. No fact may be told blank, or repeated when the new prompt
    /// already states it, while plan mode is still told under either.
    #[test_case(false ; "rendered_to_host_written")]
    #[test_case(true ; "host_written_to_rendered")]
    fn switching_who_wrote_the_prompt_tells_only_plan_mode(host_written_first: bool) {
        let mut h = Harness::new(Vec::new());
        *h.world.host_written.lock().unwrap() = host_written_first;
        h.run_in(AgentMode::Plan(PLAN_FILE.into()));
        h.world.change(|f| f.date = NEXT_DATE.into());
        *h.world.host_written.lock().unwrap() = !host_written_first;
        *h.world.authored.lock().unwrap() = "host prompt";
        h.run_in(AgentMode::Build);

        let updates = updates(&h.history);
        assert_eq!(updates.len(), 2);
        assert!(updates[0].contains(PLAN_FILE));
        assert!(updates[1].contains(PLAN_ENDED));
        assert!(!updates[1].contains(NEXT_DATE));
    }

    /// A new process keeps the stored prompt instead of rendering today's, and
    /// learns what the model last heard from the transcript alone. The frame
    /// can't help with that, because it never mentions plan mode, and a rewind
    /// can take back an update the model already got.
    #[test_case(false ; "resume")]
    #[test_case(true ; "rewind")]
    fn restored_session_continues_the_prefix(rewind: bool) {
        let mut h = Harness::new(Vec::new());
        h.run_in(AgentMode::Plan(PLAN_FILE.into()));
        let kept = h.history.len();
        h.world.change(|f| f.date = NEXT_DATE.into());
        if rewind {
            h.run_in(AgentMode::Build);
            h.requests.lock().unwrap().pop();
        }
        h.restart(kept);
        h.run_in(AgentMode::Build);

        h.assert_append_only();
        let updates = updates(&h.history);
        let last = updates.last().unwrap();
        assert!(last.contains(NEXT_DATE));
        assert!(last.contains(PLAN_ENDED));
    }

    /// Which servers are up by now is luck, so a resumed process sends the MCP
    /// tools its frame went out with and only appends the new ones.
    #[test]
    fn resumed_session_keeps_the_mcp_part_of_its_frame() {
        let mut h = Harness::new(Vec::new());
        h.model = tool_search_model();
        let reply = || vec![text_response(StopReason::EndTurn)];
        h.run(default_input(), reply(), |agent| {
            agent.mcp = Some(stub_session(&[("srv.first", "")]));
        });
        h.restart(h.history.len());
        h.run(default_input(), reply(), |agent| {
            agent.mcp = Some(stub_session(&[("srv.first", ""), ("srv.second", "")]));
        });

        h.assert_append_only();
        let requests = h.requests.lock().unwrap();
        let resumed = tool_names(&requests.last().unwrap().tools);
        assert_eq!(resumed.last(), Some(&"srv__second"));
        assert_eq!(
            h.history.frame().unwrap().mcp_tools.len(),
            resumed.len() - 1,
            "the appended tool is stored with the frame"
        );
    }

    #[test]
    fn compaction_refreshes_the_frame() {
        let mut h = Harness::new(Vec::new());
        h.run_in(AgentMode::Build);
        h.world.change(|f| f.date = NEXT_DATE.into());
        let replies = (0..3).map(|_| text_response(StopReason::EndTurn)).collect();
        h.run(default_input(), replies, with_compact);

        let requests = h.requests.lock().unwrap();
        assert_append_only(&requests[0], &requests[1]);
        assert!(requests.last().unwrap().system.contains(NEXT_DATE));
        assert!(updates(&h.history).is_empty());
    }

    /// The prompt a compaction builds can't say plan mode either. If the
    /// update was summarized away it has to be told again, and if it was
    /// carried over with the unanswered prompt it must not be told twice.
    #[test_case(false ; "summarized_away")]
    #[test_case(true ; "carried_over")]
    fn plan_mode_survives_compaction(carried: bool) {
        let (prior, replies) = if carried {
            let prior = (0..OVERFLOW_CHUNKS)
                .map(|i| Message::user(format!("{OVERFLOW_CHUNK}{i}")))
                .collect();
            (prior, 2)
        } else {
            (Vec::new(), 3)
        };
        let mut h = Harness::new(prior);
        if carried {
            h.model = small_context_model(TINY_CONTEXT_WINDOW, TINY_MAX_OUTPUT);
        }
        let input = AgentInput {
            mode: AgentMode::Plan(PLAN_FILE.into()),
            ..default_input()
        };
        let replies = (0..replies)
            .map(|_| text_response(StopReason::EndTurn))
            .collect();
        h.run(input, replies, |agent| {
            if carried {
                agent.auto_compact = true;
            } else {
                with_compact(agent);
            }
        });

        let updates = updates(&h.history);
        assert_eq!(updates.len(), 1, "{PLAN_TOLD_ONCE_MSG}");
        assert!(updates[0].contains(PLAN_FILE), "{PLAN_TOLD_ONCE_MSG}");
        let told_at = h
            .history
            .as_slice()
            .iter()
            .position(Message::is_context_update)
            .unwrap();
        let last_request = h.requests.lock().unwrap().last().unwrap().messages.len();
        assert!(told_at < last_request, "{PLAN_TOLD_ONCE_MSG}");
    }

    /// The model never sees a dropped message, so it must not see the change
    /// that came with it either. The next kept message brings it along, right
    /// in front of itself and after anything a dropped message left behind.
    #[test]
    fn dropped_message_leaves_the_change_for_the_next_kept_one() {
        let mut h = Harness::new(Vec::new());
        h.run_in(AgentMode::Build);
        h.world.change(|f| f.date = NEXT_DATE.into());
        let drop_layer = |agent: &mut Agent<'_>| {
            script(&agent.registry, drop_blocked);
        };
        let blocked = AgentInput {
            message: BLOCKED.into(),
            ..default_input()
        };
        let (reason, _) = h.run(blocked, Vec::new(), drop_layer);
        assert_eq!(reason, DoneReason::Dropped);
        assert!(updates(&h.history).is_empty());

        let burst = AgentInput {
            earlier: vec![EarlierInput {
                message: BLOCKED.into(),
                images: Vec::new(),
                preamble: vec![Message::observation(SHELL_RESULT.into())],
            }],
            ..default_input()
        };
        h.run(burst, vec![text_response(StopReason::EndTurn)], drop_layer);

        h.assert_append_only();
        let updates = updates(&h.history);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].contains(NEXT_DATE));
        let messages = h.history.as_slice();
        let told_at = messages
            .iter()
            .position(Message::is_context_update)
            .unwrap();
        assert_eq!(messages[told_at - 1].user_text(), Some(SHELL_RESULT));
        assert_eq!(
            messages[told_at + 1].user_text(),
            Some(default_input().message.as_str())
        );
    }

    /// An interrupt can switch modes. The model hears it right before the
    /// message that switched, not whenever another change comes along.
    #[test]
    fn interrupt_switching_mode_is_told_with_it() {
        let mut h = Harness::new(Vec::new());
        let interrupt = AgentInput {
            mode: AgentMode::Plan(PLAN_FILE.into()),
            ..default_input()
        };
        let replies = vec![
            tool_call_response("glob", "t1"),
            text_response(StopReason::EndTurn),
        ];
        h.run(default_input(), replies, |agent| {
            agent.interrupt_source =
                Some(MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                    vec![interrupt],
                )]));
        });

        h.assert_append_only();
        let messages = h.history.as_slice();
        let told_at = messages
            .iter()
            .position(Message::is_context_update)
            .unwrap();
        assert!(updates(&h.history)[0].contains(PLAN_FILE));
        assert!(has_interrupt_in_history(&messages[told_at + 1..]));
        let last_request = h.requests.lock().unwrap().last().unwrap().messages.len();
        assert_eq!(last_request, messages.len() - 1, "the update went out");
    }

    const CONTINUE_KEPT_MSG: &str =
        "an update is no unanswered input, so the compacted transcript must still say to continue";

    /// A switch to a smaller window compacts before the next request. The
    /// prompt the compaction builds already names the new model, so nothing
    /// is told, and the transcript still ends on the nudge to continue.
    #[test]
    fn compaction_after_a_mid_run_switch_still_continues() {
        let mut h = Harness::new(Vec::new());
        let switched = Model {
            id: SWITCHED_MODEL.into(),
            ..small_context_model(TINY_CONTEXT_WINDOW, TINY_MAX_OUTPUT)
        };
        let big_turn = StreamResponse {
            usage: TokenUsage {
                input: TINY_CONTEXT_WINDOW * 10,
                ..Default::default()
            },
            ..tool_call_response("glob", "t1")
        };
        let replies = vec![
            big_turn,
            text_response(StopReason::EndTurn),
            text_response(StopReason::EndTurn),
        ];
        let start = h.model.clone();
        h.run(default_input(), replies, |agent| {
            let provider = Arc::clone(&agent.provider);
            let slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
                model: start,
                provider: Arc::clone(&provider),
            }));
            agent.model_sync = Some(Arc::clone(&slot));
            agent.interrupt_source = Some(Arc::new(ModelPicker {
                slot,
                provider,
                model: switched,
            }));
            agent.auto_compact = true;
        });

        let continue_text = compaction::continue_message(&AgentConfig::default(), None);
        let messages = h.history.as_slice();
        assert!(
            messages
                .iter()
                .any(|m| m.first_text_content() == Some(continue_text.as_str())),
            "{CONTINUE_KEPT_MSG}"
        );
        assert!(updates(&h.history).is_empty(), "{CONTINUE_KEPT_MSG}");
        let requests = h.requests.lock().unwrap();
        assert_eq!(requests.last().unwrap().model, SWITCHED_MODEL);
    }

    /// A cut-off answer is continued by sending it back last. An update after
    /// it would get a reply instead, so the change waits for a user turn.
    #[test]
    fn change_waits_while_an_answer_is_continued() {
        let mut h = Harness::new(Vec::new());
        h.run_in(AgentMode::Build);
        h.world.change(|f| f.date = NEXT_DATE.into());
        let builder = h.world.builder();
        let (mut agent, _event_rx) = make_agent_with(
            Arc::new(MockProvider::new(Vec::new())),
            h.model.clone(),
            &mut h.history,
        );
        agent.context = builder;
        agent.refresh_frame().unwrap();

        agent.prepare_request().unwrap();
        assert!(matches!(
            agent.history.as_slice().last().unwrap().role,
            Role::Assistant
        ));
        agent.history.push(Message::user(PENDING_PROMPT.into()));
        agent.prepare_request().unwrap();
        assert!(agent.history.as_slice().last().unwrap().is_context_update());
    }

    /// The filter comes from the frame. Without one no tool may run under
    /// some other, wider set.
    #[test]
    fn tool_calls_without_a_frame_fail_closed() {
        let mut history = History::new(Vec::new());
        let (mut agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let err =
            smol::block_on(agent.process_tool_calls(tool_call_response("glob", "t1"))).unwrap_err();
        assert!(matches!(err, AgentError::Config { message } if message == NO_FRAME));
    }
    /// Two responses are the whole budget: the opening turn, then the one turn
    /// that answers all three queued messages. Answering them one by one would
    /// ask the mock for a response it does not have.
    #[test]
    fn queued_messages_are_delivered_in_one_turn() {
        smol::block_on(async {
            let inputs = Vec::from(QUEUED_MESSAGES.map(|text| AgentInput {
                message: text.into(),
                ..default_input()
            }));
            let source = MockInterruptSource::new(vec![ExtractedCommand::Interrupt(inputs)]);
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(
                MockProvider::new(vec![
                    text_response(StopReason::EndTurn),
                    text_response(StopReason::EndTurn),
                ]),
                &mut history,
            );

            let mut agent = agent.with_interrupt_source(source);
            agent.run(default_input()).await.unwrap();
            let events = drain_events(&event_rx);
            drop(agent);

            let user_texts: Vec<_> = history
                .as_slice()
                .iter()
                .filter(|m| matches!(m.role, Role::User))
                .filter_map(Message::user_text)
                .collect();
            assert_eq!(user_texts[1..], QUEUED_MESSAGES);
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e.event, AgentEvent::QueueItemConsumed { .. }))
                    .count(),
                QUEUED_MESSAGES.len()
            );
        });
    }

    #[test_case(
        (0..10).map(|i| Message::user(format!("msg {i}"))).collect(),
        vec![ExtractedCommand::Compact(None)],
        vec![tool_call_response("glob", "t1"), text_response(StopReason::EndTurn), text_response(StopReason::EndTurn)]
        ; "compaction_via_interrupt_source"
    )]
    fn compaction_through_interrupt(
        prior: Vec<Message>,
        commands: Vec<ExtractedCommand>,
        responses: Vec<StreamResponse>,
    ) {
        smol::block_on(async {
            let source = MockInterruptSource::new(commands);

            let mut history = History::new(prior);
            let (agent, _event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let result = agent
                .with_interrupt_source(source)
                .run(default_input())
                .await;

            assert!(result.is_ok());
        });
    }

    /// `TurnComplete`, `Done` and the auto-compaction trigger all report the
    /// same context number, so nobody downstream thinks it has spare room.
    #[test]
    fn context_size_is_one_gauge_across_turn_complete_and_done() {
        smol::block_on(async {
            let mut response = text_response(StopReason::EndTurn);
            response.usage = TokenUsage {
                input: 1_000,
                output: 400,
                cache_read: 250,
                cache_creation: 50,
                ..Default::default()
            };
            let expected = response.usage.total_input();
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, event_rx) = make_agent(MockProvider::new(vec![response]), &mut history);
            agent.run(default_input()).await.unwrap();
            drop(agent);

            let events = drain_events(&event_rx);
            let reported: Vec<u32> = events
                .iter()
                .filter_map(|e| match &e.event {
                    AgentEvent::TurnComplete(tc) => tc.context_size,
                    AgentEvent::Done { context_size, .. } => Some(*context_size),
                    _ => None,
                })
                .collect();
            assert_eq!(reported, vec![expected, expected], "{ONE_GAUGE_MSG}");
        });
    }

    /// The ledger banks the un-subsidised list price for metered models too,
    /// so `Done.list_cost` is the whole run rather than whatever subset of
    /// turns happened to be subsidised. `TurnComplete` stays narrow: it only
    /// carries the reference figure shown beside a `$0` bill, and a metered
    /// model has none.
    #[test]
    fn ledger_banks_list_cost_on_a_metered_model() {
        smol::block_on(async {
            let mut response = text_response(StopReason::EndTurn);
            response.usage = TokenUsage {
                input: 1_000,
                output: 400,
                cache_read: 250,
                cache_creation: 50,
                ..Default::default()
            };
            let usage = response.usage;
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, event_rx) = make_agent(MockProvider::new(vec![response]), &mut history);
            agent.run(default_input()).await.unwrap();
            drop(agent);

            let expected = default_model()
                .list_cost(&usage, false)
                .expect("the curated table prices this model");
            assert!(expected > 0.0);

            let events = drain_events(&event_rx);
            let turn_subsidised_list_cost = events.iter().find_map(|e| match &e.event {
                AgentEvent::TurnComplete(tc) => Some(tc.subsidised_list_cost),
                _ => None,
            });
            let done_list_cost = events.iter().find_map(|e| match &e.event {
                AgentEvent::Done { list_cost, .. } => Some(*list_cost),
                _ => None,
            });
            assert_eq!(turn_subsidised_list_cost, Some(None));
            assert_eq!(done_list_cost, Some(Some(expected)));
        });
    }

    #[test_case(true,  170_000, true  ; "enabled_and_over_threshold")]
    #[test_case(true,  150_000, false ; "enabled_but_below_threshold")]
    #[test_case(false, 170_000, false ; "disabled_even_over_threshold")]
    fn try_auto_compact_behavior(enabled: bool, context_size: u32, expected: bool) {
        smol::block_on(async {
            let responses = if expected {
                vec![text_response(StopReason::EndTurn)]
            } else {
                vec![]
            };
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            *agent.gauge = ContextGauge::restored(context_size);
            agent.model = Arc::new(small_context_model(200_000, 8_192));
            agent.auto_compact = enabled;
            agent.try_auto_compact().await.unwrap();
            drop(agent);
            assert_eq!(
                has_event(&drain_events(&event_rx), |e| matches!(
                    e,
                    AgentEvent::AutoCompacting { .. }
                )),
                expected,
            );
        });
    }

    const OVERFLOW_CHUNK: &str = "restored transcript chunk ";
    const OVERFLOW_CHUNKS: u32 = 400;
    const TINY_CONTEXT_WINDOW: u32 = 1_000;
    const TINY_MAX_OUTPUT: u32 = 256;
    const PENDING_PROMPT: &str = "fix the flaky test in run.rs";
    const COMPACT_FIRST_MSG: &str =
        "a resumed session over the threshold must compact before its first request";
    const PROMPT_KEPT_MSG: &str =
        "the prompt that started the run must survive the compaction verbatim";

    /// A resumed transcript that already fills the window has to be caught
    /// before the first request, and the prompt that triggered the run must
    /// not be summarized away along with it.
    #[test]
    fn resumed_overflowing_history_compacts_before_first_request() {
        smol::block_on(async {
            let prior = (0..OVERFLOW_CHUNKS)
                .map(|i| Message::user(format!("{OVERFLOW_CHUNK}{i}")))
                .collect();
            let mut history = History::new(prior);
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(vec![
                    text_response(StopReason::EndTurn),
                    text_response(StopReason::EndTurn),
                ]),
                &mut history,
            );
            agent.model = Arc::new(small_context_model(TINY_CONTEXT_WINDOW, TINY_MAX_OUTPUT));
            agent.auto_compact = true;
            agent
                .run(AgentInput {
                    message: PENDING_PROMPT.into(),
                    ..default_input()
                })
                .await
                .unwrap();
            drop(agent);

            let first = drain_events(&event_rx)
                .into_iter()
                .map(|e| e.event)
                .find(|e| {
                    matches!(
                        e,
                        AgentEvent::AutoCompacting { .. } | AgentEvent::TurnComplete(_)
                    )
                });
            assert!(
                matches!(first, Some(AgentEvent::AutoCompacting { .. })),
                "{COMPACT_FIRST_MSG}"
            );
            assert!(
                history
                    .as_slice()
                    .iter()
                    .flat_map(|m| &m.content)
                    .any(|b| matches!(b, ContentBlock::Text { text } if text == PENDING_PROMPT)),
                "{PROMPT_KEPT_MSG}"
            );
        });
    }

    const OVERFLOW_STATUS: u16 = 413;
    const OVERFLOW_MESSAGE: &str = "prompt is too long";
    const RECOVER_MSG: &str = "an overflow the gauge missed must compact and retry";
    const GIVE_UP_MSG: &str = "a second overflow in a row must surface, not compact again";

    /// Overflows the next `0` requests, then behaves.
    struct OverflowProvider(Mutex<u32>);

    impl Provider for OverflowProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                let mut remaining = self.0.lock().unwrap();
                match remaining.checked_sub(1) {
                    Some(rest) => {
                        *remaining = rest;
                        Err(AgentError::api(OVERFLOW_STATUS, OVERFLOW_MESSAGE))
                    }
                    None => Ok(text_response(StopReason::EndTurn)),
                }
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// The gauge is an estimate, so a prompt can overflow with the threshold
    /// still unmet. That is what compaction is for, but only once: a second
    /// overflow means it did not help.
    #[test_case(0, true, RECOVER_MSG ; "unpredicted_overflow_compacts_and_retries")]
    #[test_case(MAX_OVERFLOW_RECOVERIES, false, GIVE_UP_MSG ; "exhausted_recoveries_surface_the_error")]
    fn overflow_recovery(recoveries: u32, expected: bool, message: &str) {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, event_rx) = make_agent(OverflowProvider(Mutex::new(1)), &mut history);
            agent.auto_compact = true;
            agent.overflow_recoveries = recoveries;

            let recovered = agent.run(default_input()).await.is_ok();
            drop(agent);

            assert_eq!(recovered, expected, "{message}");
            assert_eq!(
                has_event(&drain_events(&event_rx), |e| matches!(
                    e,
                    AgentEvent::AutoCompacting { .. }
                )),
                expected,
                "{message}"
            );
        });
    }

    const UNBOUND_MESSAGE: &str = "messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.";
    const UNBOUND_RECOVER_MSG: &str =
        "thinking bound to a moved prefix must be dropped once and the turn retried";
    const UNBOUND_GIVE_UP_MSG: &str = "with no thinking left to drop, the error must surface";

    /// Fails while a request still carries thinking, or always.
    struct UnboundThinkingProvider {
        always: bool,
    }

    impl Provider for UnboundThinkingProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            let thinking = messages
                .iter()
                .flat_map(|m| &m.content)
                .any(ContentBlock::is_thinking);
            Box::pin(async move {
                if self.always || thinking {
                    Err(AgentError::api(400, UNBOUND_MESSAGE))
                } else {
                    Ok(text_response(StopReason::EndTurn))
                }
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    #[test_case(false, true, UNBOUND_RECOVER_MSG ; "drops_thinking_and_retries")]
    #[test_case(true, false, UNBOUND_GIVE_UP_MSG ; "nothing_left_to_drop_surfaces_the_error")]
    fn unbound_thinking_recovery(always: bool, expected: bool, message: &str) {
        smol::block_on(async {
            let mut history = History::new(vec![
                Message::user("go".into()),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "hmm".into(),
                            signature: Some("sig".into()),
                        },
                        ContentBlock::Text {
                            text: "done".into(),
                        },
                    ],
                    ..Default::default()
                },
            ]);
            let (mut agent, event_rx) =
                make_agent(UnboundThinkingProvider { always }, &mut history);

            let recovered = agent.run(default_input()).await.is_ok();
            drop(agent);

            assert_eq!(recovered, expected, "{message}");
            assert!(
                history
                    .as_slice()
                    .iter()
                    .flat_map(|m| &m.content)
                    .all(|b| !b.is_thinking()),
                "{message}"
            );
            assert!(
                has_event(&drain_events(&event_rx), |e| matches!(
                    e,
                    AgentEvent::Notice { text } if text == THINKING_DROPPED
                )),
                "{message}"
            );
        });
    }

    const QUEUED_INPUT: &str = "actually, check the other module first";
    const QUEUED_COMPACTED_MSG: &str = "the filled gauge must trigger a compaction";
    const QUEUED_KEPT_MSG: &str = "input queued mid-run must survive the compaction verbatim";

    /// The turn before it already appended an assistant message, so the tail
    /// alone cannot tell that this input is still unanswered.
    #[test]
    fn compaction_carries_input_queued_mid_run() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let filling = StreamResponse {
                usage: TokenUsage {
                    input: TINY_CONTEXT_WINDOW,
                    ..Default::default()
                },
                ..text_response(StopReason::EndTurn)
            };
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(vec![
                    filling,
                    text_response(StopReason::EndTurn),
                    text_response(StopReason::EndTurn),
                ]),
                &mut history,
            );
            agent.model = Arc::new(small_context_model(TINY_CONTEXT_WINDOW, TINY_MAX_OUTPUT));
            agent.auto_compact = true;
            agent.interrupt_source =
                Some(MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                    vec![AgentInput {
                        message: QUEUED_INPUT.into(),
                        ..default_input()
                    }],
                )]));
            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert!(
                has_event(&drain_events(&event_rx), |e| matches!(
                    e,
                    AgentEvent::AutoCompacting { .. }
                )),
                "{QUEUED_COMPACTED_MSG}"
            );
            assert!(
                history.as_slice().iter().flat_map(|m| &m.content).any(
                    |b| matches!(b, ContentBlock::Text { text } if text.contains(QUEUED_INPUT))
                ),
                "{QUEUED_KEPT_MSG}"
            );
        });
    }

    #[test]
    fn do_compact_appends_post_instructions_to_continue_message() {
        smol::block_on(async {
            const POST: &str = "Re-read plan.md";
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.config.post_compaction_instructions = Some(POST.into());
            agent.carry_from = agent.history.len();
            agent.do_compact(CompactSteer::default()).await.unwrap();
            drop(agent);

            let last = history.as_slice().last().unwrap();
            assert!(matches!(
                &last.content[0],
                ContentBlock::Text { text } if text.ends_with(POST) && text != POST
            ));
        });
    }

    #[test]
    fn cancel_token_aborts_during_api_call() {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            trigger.cancel();

            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(StubStreamProvider::default(), &mut history);
            let mut agent = agent.with_cancel(cancel);

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            drop(agent);
            assert_ends_with_cancel_marker(&history);
            assert!(has_event(&drain_events(&event_rx), |e| matches!(
                e,
                AgentEvent::Done {
                    reason: DoneReason::Cancelled,
                    ..
                }
            )));
        });
    }

    #[test]
    fn cancel_mid_stream_keeps_partial_text_in_history() {
        const PARTIAL: &str = "partial answer";
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let provider = StubStreamProvider {
                delta: Some(PARTIAL),
                cancel_after_delta: Mutex::new(Some(trigger)),
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_cancel(cancel);

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            drop(agent);
            assert_ends_with_cancel_marker(&history);
            let messages = history.as_slice();
            let partial = &messages[messages.len() - 2];
            assert!(matches!(partial.role, Role::Assistant));
            let expected = format!("{PARTIAL}\n\n{CANCELLED_TEXT_NOTE}");
            assert!(
                matches!(&partial.content[0], ContentBlock::Text { text } if *text == expected),
                "kept text must carry the truncation note so the model never resumes it"
            );
        });
    }

    /// The `Retry` event already made the view drop the failed attempt's
    /// text, so history must not resurrect it (see `StreamError`).
    #[test]
    fn cancel_during_retry_backoff_discards_failed_attempt_text() {
        const PARTIAL: &str = "doomed attempt";
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let provider = StubStreamProvider {
                delta: Some(PARTIAL),
                fail_status: Some(529),
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_cancel(cancel);

            let mut trigger = Some(trigger);
            let pump = smol::spawn(async move {
                while let Ok(envelope) = event_rx.recv_async().await {
                    if matches!(envelope.event, AgentEvent::Retry { .. })
                        && let Some(t) = trigger.take()
                    {
                        t.cancel();
                    }
                }
            });

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            drop(agent);
            pump.await;

            assert_ends_with_cancel_marker(&history);
            assert!(
                history
                    .as_slice()
                    .iter()
                    .all(|m| !m.content.iter().any(
                        |b| matches!(b, ContentBlock::Text { text } if text.contains(PARTIAL))
                    )),
                "failed attempt's text must not reach history"
            );
        });
    }

    #[test_case(
        vec![tool_call_response("nonexistent_tool_xyz", "t1"), text_response(StopReason::EndTurn)],
        "t1"
        ; "parse_error"
    )]
    #[test_case(
        vec![tool_call_response("glob", "t1"), tool_call_response("glob", "t2"), tool_call_response("glob", "t3"), text_response(StopReason::EndTurn)],
        "t3"
        ; "doom_loop"
    )]
    fn error_emits_tool_done_event(responses: Vec<StreamResponse>, expected_error_id: &str) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            assert!(has_event(&events, |e| matches!(
                e,
                AgentEvent::ToolDone(done) if done.is_error && done.id == expected_error_id
            )));
        });
    }

    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            empty_response(),
            text_response(StopReason::EndTurn),
        ],
        3, 1
        ; "nudge_on_empty_after_tools"
    )]
    #[test_case(
        [tool_call_response("glob", "t1"), thinking_response()]
            .into_iter()
            .chain((0..MAX_NUDGES).map(|_| empty_response()))
            .collect(),
        MAX_NUDGES + 2, MAX_NUDGES as usize
        ; "gives_up_after_max_nudges"
    )]
    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            text_response(StopReason::EndTurn),
        ],
        2, 0
        ; "no_nudge_when_text_after_tools"
    )]
    #[test_case(
        vec![
            empty_response(),
            text_response(StopReason::EndTurn),
        ],
        1, 0
        ; "no_nudge_without_recent_tools"
    )]
    fn nudge_behavior(responses: Vec<StreamResponse>, expected_turns: u32, expected_nudges: usize) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            let nudges = events
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::Nudge))
                .count();
            assert_eq!(nudges, expected_nudges);

            let done = events
                .iter()
                .find_map(|e| match &e.event {
                    AgentEvent::Done { num_turns, .. } => Some(*num_turns),
                    _ => None,
                })
                .expect("expected Done event");
            assert_eq!(done, expected_turns);

            assert!(
                history
                    .as_slice()
                    .iter()
                    .all(|m| m.content.iter().any(|b| !b.is_thinking())),
                "history holds a message no provider will accept: {:?}",
                history.as_slice()
            );
        });
    }

    /// Pins the regression where a stale nudge counter made a follow-up
    /// "continue" end instantly: the budget lives in the history tail, and
    /// the new user message breaks the streak.
    #[test]
    fn nudge_budget_resets_on_new_run() {
        smol::block_on(async {
            let responses = [tool_call_response("glob", "t1")]
                .into_iter()
                .chain((0..=MAX_NUDGES).map(|_| empty_response()))
                .chain([empty_response(), text_response(StopReason::EndTurn)])
                .collect();
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let _ = agent.run(default_input()).await;
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            let nudges = events
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::Nudge))
                .count();
            assert_eq!(nudges, MAX_NUDGES as usize + 1);
        });
    }

    /// Wiring this to `None` to make the struct literal compile would
    /// silently reintroduce the bug the field exists to fix.
    #[test]
    fn tool_context_carries_the_session() {
        let mut history = History::new(Vec::new());
        let (mut agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
        assert_eq!(agent.tool_context(Arc::default()).session_id, None);

        let session: SessionRef = "01965087-4c71-7f00-8000-000000000000"
            .parse()
            .expect("valid session id");
        agent.session_id = Some(session.clone());
        assert_eq!(agent.tool_context(Arc::default()).session_id, Some(session));
    }

    fn answer_only(
        slot: AgentSlot,
        value: Value,
    ) -> impl Fn(AgentSlot, &Value) -> Verdict + Send + Sync + 'static {
        move |fired, _| {
            if fired == slot {
                Verdict::Replaced(value.clone())
            } else {
                Verdict::Unchanged
            }
        }
    }

    fn steers(events: &[Envelope]) -> Vec<(SteerKind, String)> {
        events
            .iter()
            .filter_map(|e| match &e.event {
                AgentEvent::Steered { kind, text } => Some((*kind, text.clone())),
                _ => None,
            })
            .collect()
    }

    const REWRITTEN: &str = "hello, with the CI log attached";
    const DROP_REASON: &str = "that prompt is on the blocklist";
    const KEEP_GOING: &str = "The todo list still has open items.";
    const BLOCKED: &str = "blocked";
    const SHELL_RESULT: &str = "shell result";

    fn drop_blocked(slot: AgentSlot, value: &Value) -> Verdict {
        match slot {
            AgentSlot::UserMessage if value[FIELD_TEXT] == BLOCKED => {
                Verdict::Denied(DROP_REASON.into())
            }
            AgentSlot::CompactBefore => Verdict::Replaced(json!({ compaction::FIELD_SKIP: true })),
            _ => Verdict::Unchanged,
        }
    }

    #[test]
    fn user_message_rewrite_is_what_the_model_sees() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let seen = script(
                &agent.registry,
                answer_only(AgentSlot::UserMessage, json!({ FIELD_TEXT: REWRITTEN })),
            );

            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(history.as_slice()[0].user_text(), Some(REWRITTEN));
            let (slot, value) = seen.lock().unwrap()[0].clone();
            assert_eq!(slot, AgentSlot::UserMessage);
            assert_eq!(value["source"], InputSource::Tui.as_str());
            assert_eq!(
                steers(&drain_events(&event_rx)),
                [(SteerKind::MessageRewritten, REWRITTEN.to_owned())]
            );
        });
    }

    /// The mock has no responses and panics on any request, so passing proves
    /// the model never heard the message.
    #[test_case(|_, _| Verdict::Denied(DROP_REASON.into()), DROP_REASON ; "denied")]
    #[test_case(|_, _| Verdict::Replaced(json!({ FIELD_TEXT: " " })), EMPTIED_MESSAGE ; "rewritten_to_nothing")]
    fn user_message_drop_ends_the_run_before_any_request(
        answer: fn(AgentSlot, &Value) -> Verdict,
        reason: &str,
    ) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            script(&agent.registry, answer);

            let done = agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(done, DoneReason::Dropped);
            assert!(history.is_empty());
            assert_eq!(
                steers(&drain_events(&event_rx)),
                [(SteerKind::MessageDropped, reason.to_owned())]
            );
        });
    }

    /// The mock holds one response, so a run that went on anyway would panic
    /// on its second request.
    #[test_case(ExtractedCommand::Interrupt(vec![AgentInput { message: BLOCKED.into(), ..default_input() }]) ; "dropped_interrupt")]
    #[test_case(ExtractedCommand::Compact(None) ; "skipped_compact")]
    fn queued_command_that_lands_nothing_ends_the_run(cmd: ExtractedCommand) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let mut agent = agent.with_interrupt_source(MockInterruptSource::new(vec![cmd]));
            script(&agent.registry, drop_blocked);

            let reason = agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(reason, DoneReason::EndTurn);
            assert!(matches!(
                history.as_slice().last().unwrap().role,
                Role::Assistant
            ));
        });
    }

    /// The shell result lands in every case, even when every message around
    /// it is dropped. The user already ran that command, so it is not a layer's
    /// call to make.
    #[test_case(BLOCKED, "hello", &[SHELL_RESULT, "hello"], DoneReason::EndTurn ; "earlier_dropped")]
    #[test_case("first", BLOCKED, &[SHELL_RESULT, "first"], DoneReason::EndTurn ; "last_dropped")]
    #[test_case(BLOCKED, BLOCKED, &[SHELL_RESULT], DoneReason::Dropped ; "all_dropped")]
    fn burst_filters_every_message(
        first: &str,
        last: &str,
        expected: &[&str],
        expected_reason: DoneReason,
    ) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            script(&agent.registry, drop_blocked);
            let input = AgentInput {
                message: last.into(),
                earlier: vec![EarlierInput {
                    message: first.into(),
                    images: Vec::new(),
                    preamble: vec![Message::observation(SHELL_RESULT.into())],
                }],
                ..default_input()
            };

            let reason = agent.run(input).await.unwrap();
            drop(agent);

            assert_eq!(reason, expected_reason);
            let users: Vec<_> = history
                .as_slice()
                .iter()
                .filter(|m| matches!(m.role, Role::User))
                .filter_map(Message::user_text)
                .collect();
            assert_eq!(users, expected);
        });
    }

    /// A layer that always says "continue" is exactly what both bounds are
    /// for. The mock holds one turn past what the bound allows, so a missing
    /// bound panics instead of looping.
    #[test_case(None, MAX_STOP_CONTINUATIONS ; "capped_in_a_row")]
    #[test_case(Some(1), 0 ; "not_asked_on_the_last_turn")]
    fn stop_continuations_are_bounded(max_turns: Option<u32>, allowed: u32) {
        smol::block_on(async {
            let allowed = allowed as usize;
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(
                    (0..=allowed)
                        .map(|_| text_response(StopReason::EndTurn))
                        .collect(),
                ),
                &mut history,
            );
            agent.config.max_turns = max_turns;
            script(
                &agent.registry,
                answer_only(AgentSlot::Stop, json!({ FIELD_CONTINUE: KEEP_GOING })),
            );

            let reason = agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(reason, DoneReason::EndTurn);
            assert_continued(&drain_events(&event_rx), &history, allowed);
        });
    }

    /// The user is told about every continuation and the model reads each one.
    fn assert_continued(events: &[Envelope], history: &History, expected: usize) {
        assert_eq!(steers(events).len(), expected);
        let continued = history
            .as_slice()
            .iter()
            .filter(|m| m.first_text_content() == Some(KEEP_GOING))
            .count();
        assert_eq!(continued, expected);
    }

    /// A slow stop layer is the widest window a cancel can land in after the
    /// answer is done, and the frontend must still hear it was cancelled.
    #[test]
    fn cancel_during_stop_layer_reports_cancelled() {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let trigger = Mutex::new(Some(trigger));
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let mut agent = agent.with_cancel(cancel);
            script(&agent.registry, move |slot, _| {
                if slot == AgentSlot::Stop
                    && let Some(trigger) = trigger.lock().unwrap().take()
                {
                    trigger.cancel();
                }
                Verdict::Unchanged
            });

            let reason = agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(reason, DoneReason::Cancelled);
            assert_ends_with_cancel_marker(&history);
        });
    }

    /// The mock has no responses, so a message that got through would panic
    /// on the request.
    #[test]
    fn cancel_during_user_message_layer_keeps_the_message_out() {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let trigger = Mutex::new(Some(trigger));
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let mut agent = agent.with_cancel(cancel);
            script(&agent.registry, move |_, _| {
                if let Some(trigger) = trigger.lock().unwrap().take() {
                    trigger.cancel();
                }
                Verdict::Unchanged
            });

            let reason = agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(reason, DoneReason::Cancelled);
            assert!(history.is_empty());
        });
    }

    #[test]
    fn stop_sees_the_last_answer() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let seen = script(&agent.registry, |_, _| Verdict::Unchanged);

            agent.run(default_input()).await.unwrap();

            let seen = seen.lock().unwrap();
            let (_, stop) = seen.iter().find(|(s, _)| *s == AgentSlot::Stop).unwrap();
            assert_eq!(stop["reason"], STOP_FINISHED);
            assert_eq!(stop["last_message"], RESPONSE_TEXT);
            assert_eq!(stop["num_turns"], 1);
        });
    }

    #[test_case(drop_blocked, CompactReason::Auto, false ; "auto_is_skipped")]
    #[test_case(drop_blocked, CompactReason::Overflow, true ; "overflow_ignores_the_skip")]
    #[test_case(|_, _| Verdict::Denied(DROP_REASON.into()), CompactReason::Auto, false ; "denied_auto_is_skipped")]
    #[test_case(|_, _| Verdict::Denied(DROP_REASON.into()), CompactReason::Overflow, true ; "denied_overflow_still_compacts")]
    fn compact_before_skip(
        answer: fn(AgentSlot, &Value) -> Verdict,
        reason: CompactReason,
        compacts: bool,
    ) {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            script(&agent.registry, answer);

            agent.compact_now(reason).await.unwrap();
            drop(agent);

            assert_eq!(
                has_event(&drain_events(&event_rx), |e| matches!(
                    e,
                    AgentEvent::CompactionDone { .. }
                )),
                compacts
            );
        });
    }

    #[test]
    fn compact_before_continue_joins_the_configured_one() {
        smol::block_on(async {
            const POST: &str = "Re-read plan.md";
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.config.post_compaction_instructions = Some(POST.into());
            agent.carry_from = agent.history.len();
            script(
                &agent.registry,
                answer_only(
                    AgentSlot::CompactBefore,
                    json!({ FIELD_CONTINUE: KEEP_GOING }),
                ),
            );

            agent.compact_now(CompactReason::Auto).await.unwrap();
            drop(agent);

            let last = history
                .as_slice()
                .last()
                .unwrap()
                .first_text_content()
                .unwrap();
            assert!(last.contains(POST) && last.contains(KEEP_GOING), "{last}");
        });
    }

    const CARRIED: &str = "answer this next";
    const SUMMARY: &str = "We fixed the flaky test in run.rs";

    /// The carried prompt already says what to do next, so the layer's words
    /// follow it and the generic nudge stays out.
    #[test]
    fn compact_before_continue_follows_carried_input() {
        smol::block_on(async {
            let mut history = History::new(vec![
                Message::user("go".into()),
                Message::user(CARRIED.into()),
            ]);
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.carry_from = 1;
            script(
                &agent.registry,
                answer_only(
                    AgentSlot::CompactBefore,
                    json!({ FIELD_CONTINUE: KEEP_GOING }),
                ),
            );

            agent.compact_now(CompactReason::Auto).await.unwrap();
            drop(agent);

            let tail: Vec<_> = history.as_slice()[2..]
                .iter()
                .map(Message::first_text_content)
                .collect();
            assert_eq!(tail, [Some(CARRIED), Some(KEEP_GOING)]);
        });
    }

    #[test]
    fn compaction_done_carries_the_summary() {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(vec![assistant_response(vec![ContentBlock::Text {
                    text: SUMMARY.into(),
                }])]),
                &mut history,
            );
            agent.carry_from = agent.history.len();

            agent.do_compact(CompactSteer::default()).await.unwrap();
            drop(agent);

            let summaries: Vec<_> = drain_events(&event_rx)
                .into_iter()
                .filter_map(|e| match e.event {
                    AgentEvent::CompactionDone { summary, .. } => Some(summary),
                    _ => None,
                })
                .collect();
            assert_eq!(summaries, [SUMMARY]);
        });
    }

    /// Lands its command once the stop layer has spent the allowance, keyed on
    /// what the layer was asked rather than on how often the loop polls.
    struct InterruptWhenSpent {
        seen: Seen,
        cmd: Mutex<Option<ExtractedCommand>>,
    }

    impl InterruptSource for InterruptWhenSpent {
        fn poll(&self) -> Option<ExtractedCommand> {
            let stops = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(slot, _)| *slot == AgentSlot::Stop)
                .count();
            (stops >= MAX_STOP_CONTINUATIONS as usize)
                .then(|| self.cmd.lock().unwrap().take())
                .flatten()
        }
    }

    /// The mock holds exactly two allowances' worth of turns plus the one the
    /// interrupt adds, so a missing reset ends the run early and a missing
    /// bound panics on the request after.
    #[test]
    fn interrupt_restores_the_stop_allowance() {
        smol::block_on(async {
            let allowed = 2 * MAX_STOP_CONTINUATIONS as usize;
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(
                MockProvider::new(
                    (0..allowed + 2)
                        .map(|_| text_response(StopReason::EndTurn))
                        .collect(),
                ),
                &mut history,
            );
            let seen = script(
                &agent.registry,
                answer_only(AgentSlot::Stop, json!({ FIELD_CONTINUE: KEEP_GOING })),
            );
            let mut agent = agent.with_interrupt_source(Arc::new(InterruptWhenSpent {
                seen,
                cmd: Mutex::new(Some(ExtractedCommand::Interrupt(vec![AgentInput {
                    message: QUEUED_INPUT.into(),
                    ..default_input()
                }]))),
            }));

            let reason = agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(reason, DoneReason::EndTurn);
            assert_continued(&drain_events(&event_rx), &history, allowed);
        });
    }
}
