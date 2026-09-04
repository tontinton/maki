use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tracing::{error, info, warn};

use maki_providers::provider::Provider;
use maki_providers::{
    ContentBlock, Message, Model, RequestOptions, Role, StopReason, StreamResponse, TokenUsage,
};

use super::compaction;
use super::history::{History, sanitize_cancelled_history};
use super::instructions::LoadedInstructions;
use super::streaming::{StreamError, stream_with_retry};
use super::tool_dispatch::{self, RecentCalls};
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::McpSession;
use crate::permissions::PermissionManager;
use crate::tools::{Deadline, FileAccess, LocalTools, RequestTools, ToolAudience, ToolContext};
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, DoneReason, EventSender,
    ExtractedCommand, InterruptSource, RunLedger, SessionMailbox, TurnCompleteEvent,
};
use maki_config::{ModelPolicy, ToolOutputLines};
use maki_storage::id::SessionRef;

const MAX_REAUTH_ATTEMPTS: u32 = 2;
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
    pub system: String,
    pub event_tx: EventSender,
    pub tools: RequestTools,
}

pub struct Agent<'h> {
    provider: Arc<dyn Provider>,
    model: Arc<Model>,
    history: &'h mut History,
    system: String,
    event_tx: EventSender,
    tools: RequestTools,
    mode: AgentMode,
    user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
    ledger: Arc<RunLedger>,
    context_size: u32,
    num_turns: u32,
    recent_calls: RecentCalls,
    auto_compact: bool,
    loaded_instructions: LoadedInstructions,
    rollback_len: usize,
    mcp: Option<McpSession>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    reauth_attempts: u32,
    permissions: Arc<PermissionManager>,
    opts: RequestOptions,
    session_id: Option<SessionRef>,
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
            system: run.system,
            event_tx: run.event_tx,
            tools: run.tools,
            mode: AgentMode::default(),
            user_response_rx: None,
            interrupt_source: None,
            cancel: CancelToken::none(),
            ledger: params.ledger,
            context_size: 0,
            num_turns: 0,
            recent_calls: RecentCalls::new(),
            auto_compact: compaction::auto_compact_enabled(),
            loaded_instructions: LoadedInstructions::new(),
            rollback_len: 0,
            mcp: None,
            reauth_attempts: 0,
            opts: RequestOptions::default(),
            session_id: params.session_id,
            mailbox: params.mailbox,
            file_access: params.file_access,
            prompt_slots: params.prompt_slots,
            subagent_cancels: params.subagent_cancels,
            registry: params.registry,
            audience: params.audience,
            workflow: false,
            local_tools: LocalTools::default(),
            model_policy: params.model_policy,
        }
    }

    pub fn with_mcp(mut self, mcp: Option<McpSession>) -> Self {
        self.mcp = mcp;
        self
    }

    pub fn with_user_response_rx(
        mut self,
        rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
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
            thinking,
            fast,
            workflow,
            prompt: _,
        } = input;
        self.permissions.reset_review_turn();
        self.rollback_len = self.history.len();
        self.push_input_context(preamble);
        if !message.trim().is_empty() || !images.is_empty() {
            self.history
                .push(Message::user_with_images(message.clone(), images));
        }
        self.mode = mode;
        self.workflow = workflow;
        self.opts = RequestOptions { thinking, fast };

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

        // Every frontend enters here, so busy time is measured here; a turn
        // that failed was still busy.
        let busy_since = Instant::now();
        let result = self.run_loop().await;
        if top_level {
            maki_otel::emit::active_time(busy_since.elapsed());
        }
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

    fn push_input_context(&mut self, preamble: Vec<Message>) {
        for message in preamble {
            self.history.push(message);
        }
        if let Some(mailbox) = &self.mailbox {
            for message in mailbox.drain() {
                self.history.push(message);
            }
        }
    }

    async fn run_loop(&mut self) -> Result<DoneReason, AgentError> {
        loop {
            if let Some(max) = self.config.max_turns
                && self.num_turns >= max
            {
                return Ok(DoneReason::MaxTurns);
            }
            match self.turn().await? {
                TurnOutcome::Continue => {}
                TurnOutcome::Done(reason) => return Ok(reason),
            }
        }
    }

    /// `self.tools` holds base tools only; the MCP part is recomputed here
    /// every turn so `tool_search` loads and late-connecting servers take
    /// effect on the next request.
    fn request_tools(&self) -> Cow<'_, Value> {
        match &self.mcp {
            Some(mcp) => {
                let mut tools = self.tools.definitions().clone();
                mcp.extend_tools(&mut tools);
                Cow::Owned(tools)
            }
            None => Cow::Borrowed(self.tools.definitions()),
        }
    }

    async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let tools = self.request_tools();
        let response = match stream_with_retry(
            &*self.provider,
            &self.model,
            self.history.as_slice(),
            &self.system,
            tools.as_ref(),
            &self.event_tx,
            &self.cancel,
            self.opts,
            self.session_id.as_ref(),
        )
        .await
        {
            Ok(r) => {
                self.reauth_attempts = 0;
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
            Err(StreamError::Other(e)) => {
                error!(error = %e, model = %self.model.id, self.num_turns, "stream_message failed");
                return Err(e);
            }
        };
        self.num_turns += 1;

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

        self.context_size = response.usage.total_input();
        self.emit_turn_complete(&response)?;

        if has_tools {
            let history_len_before = self.history.len();
            self.process_tool_calls(response).await?;
            self.context_size +=
                estimate_message_tokens(&self.history.as_slice()[history_len_before..]);
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

        if self.try_auto_compact().await? || self.handle_queued_command().await? {
            return Ok(TurnOutcome::Continue);
        }

        if has_tools {
            Ok(TurnOutcome::Continue)
        } else {
            Ok(TurnOutcome::Done(stop_reason.into()))
        }
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
        let fast = self.opts.clamped(&self.model).fast;
        let cost = self.model.billed_cost(&response.usage, fast);
        // Ledger and TurnComplete both use the subsidised list price so a
        // plugin summing per-turn `list_cost` from events lands on the same
        // total the ledger later hands to `Done`.
        let list_cost = self.model.subsidised_list_cost(&response.usage, fast);
        self.ledger.add(response.usage, cost, list_cost);
        self.event_tx
            .send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: response.message.clone(),
                usage: response.usage,
                model: self.model.id.clone(),
                cost,
                list_cost,
                context_size: Some(self.context_size),
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
            context_size: self.context_size,
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

    async fn process_tool_calls(&mut self, response: StreamResponse) -> Result<(), AgentError> {
        let ctx = self.tool_context();
        tool_dispatch::process_tool_calls(
            response,
            &mut self.recent_calls,
            self.history,
            &self.event_tx,
            &ctx,
        )
        .await
    }

    fn tool_context(&self) -> ToolContext {
        ToolContext {
            provider: Arc::clone(&self.provider),
            model: Arc::clone(&self.model),
            event_tx: self.event_tx.clone(),
            mode: self.mode.clone(),
            session_id: self.session_id.clone(),
            tool_use_id: None,
            user_response_rx: self.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
            deadline: Deadline::None,
            config: self.config.clone(),
            tool_filter: Arc::clone(self.tools.filter()),
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
            recent_user_messages: self
                .history
                .recent_user_texts(crate::reviewers::REVIEW_CONTEXT_MESSAGES)
                .into_iter()
                .map(Arc::from)
                .collect(),
        }
    }

    async fn try_auto_compact(&mut self) -> Result<bool, AgentError> {
        if !self.auto_compact
            || !compaction::is_overflow(
                &TokenUsage {
                    input: self.context_size,
                    ..Default::default()
                },
                &self.model,
                self.config.compaction_buffer,
            )
        {
            return Ok(false);
        }
        info!(context_size = self.context_size, "auto-compacting");
        self.event_tx.send(AgentEvent::AutoCompacting {
            context_size: self.context_size,
            context_window: self.model.context_window,
        })?;
        self.do_compact(None).await?;
        Ok(true)
    }

    async fn do_compact(&mut self, instructions: Option<&str>) -> Result<(), AgentError> {
        let context_size_before = self.context_size;
        let (compact_provider, compact_model) = resolve_compaction_model(
            &self.provider,
            &self.model,
            self.timeouts,
            &self.model_policy,
        );
        let compaction_usage = compaction::compact_history(
            &*compact_provider,
            &compact_model,
            self.history,
            &self.event_tx,
            &self.cancel,
            &self.config,
            instructions,
        )
        .await?;
        // The summariser can be a different model, so price this with
        // `compact_model` and not `self.model`.
        let fast = self.opts.clamped(&compact_model).fast;
        let compact_cost = compact_model.billed_cost(&compaction_usage, fast);
        let compact_list_cost = compact_model.list_cost(&compaction_usage, fast);
        self.ledger
            .add(compaction_usage, compact_cost, compact_list_cost);
        // The summary the model just wrote is all the next call will see, so
        // its output count is the new gauge.
        let context_size_after = compaction_usage.output;
        self.context_size = context_size_after;
        self.rollback_len = self.history.len();
        self.event_tx.send(AgentEvent::CompactionDone {
            context_size_before,
            context_size_after,
            context_window: self.model.context_window,
        })?;
        self.history
            .push(Message::synthetic(compaction::continue_message(
                &self.config,
            )));
        Ok(())
    }

    async fn handle_queued_command(&mut self) -> Result<bool, AgentError> {
        let Some(ref source) = self.interrupt_source else {
            return Ok(false);
        };
        let Some(cmd) = source.poll() else {
            return Ok(false);
        };
        match cmd {
            // The burst lands as consecutive user messages, so one request
            // carries all of it.
            ExtractedCommand::Interrupt(inputs) => {
                for input in inputs {
                    self.event_tx.send(AgentEvent::QueueItemConsumed {
                        text: input.message.clone(),
                        image_count: input.images.len(),
                    })?;
                    self.push_input_context(input.preamble);
                    self.mode = input.mode;
                    let wrapped = format!(
                        "<user-interrupt>\n{INTERRUPT_NOTE}\n\n{}\n</user-interrupt>",
                        input.message
                    );
                    self.history.push(Message {
                        display_text: Some(input.message),
                        ..Message::user_with_images(wrapped, input.images)
                    });
                }
            }
            ExtractedCommand::Compact(instructions) => {
                self.do_compact(instructions.as_deref()).await?;
            }
        }
        Ok(true)
    }
}

const CHARS_PER_TOKEN: usize = 4;

/// Counts message content only. The system prompt and the tool schemas, a five
/// figure baseline on a full tool set, stay invisible here, so never let this
/// replace a context size the provider measured.
pub fn estimate_message_tokens(messages: &[Message]) -> u32 {
    if messages.is_empty() {
        return 0;
    }
    let total_bytes: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.len()),
            ContentBlock::ToolResult { content, .. } => Some(content.len()),
            ContentBlock::ToolUse { input, .. } => Some(input.to_string().len()),
            ContentBlock::Thinking { thinking, .. } => Some(thinking.len()),
            _ => None,
        })
        .sum();
    (total_bytes.max(CHARS_PER_TOKEN) / CHARS_PER_TOKEN) as u32
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use maki_providers::provider::{BoxFuture, Provider};
    use maki_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::Envelope;
    use crate::mcp::tool_names;
    use crate::permissions::PermissionManager;

    const QUEUED_MESSAGES: [&str; 3] = ["first", "second", "third"];
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

    struct MockProvider {
        responses: Mutex<Vec<StreamResponse>>,
        captured_tools: Arc<Mutex<Vec<Value>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                captured_tools: Arc::default(),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            tools: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                self.captured_tools.lock().unwrap().push(tools.clone());
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
                    Some(status) => Err(AgentError::Api {
                        status,
                        message: "stub".into(),
                    }),
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
        }
    }

    fn make_agent(
        provider: impl Provider + 'static,
        history: &mut History,
    ) -> (Agent<'_>, flume::Receiver<Envelope>) {
        let (raw_tx, event_rx) = flume::unbounded();
        let agent = Agent::new(
            AgentParams {
                provider: Arc::new(provider),
                model: default_model(),
                config: AgentConfig::default(),
                tool_output_lines: ToolOutputLines::default(),
                permissions: Arc::new(PermissionManager::new(
                    maki_config::PermissionsConfig {
                        default: maki_config::DefaultEffect::Allow,
                        rules: vec![],
                        ..Default::default()
                    },
                    std::path::PathBuf::from("/tmp"),
                    Arc::default(),
                )),
                session_id: None,
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
                system: "system".into(),
                event_tx: EventSender::new(raw_tx, 0),
                tools: RequestTools::default(),
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
            thinking: Default::default(),
            fast: false,
            workflow: false,
            prompt: None,
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
        }
    }

    #[test]
    fn mcp_definitions_refresh_per_request() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![
                tool_use_response(
                    crate::mcp::TOOL_SEARCH_TOOL_NAME,
                    serde_json::json!({"query": "fetch issue"}),
                ),
                text_response(StopReason::EndTurn),
            ]);
            let captured = Arc::clone(&provider.captured_tools);
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_mcp(Some(crate::mcp::test_support::stub_session(&[(
                "srv.fetch_issue",
                "Fetch a GitHub issue",
            )])));
            agent.run(default_input()).await.unwrap();

            let captured = captured.lock().unwrap();
            assert_eq!(captured.len(), 2);
            let first = tool_names(&captured[0]);
            assert!(first.contains(&crate::mcp::TOOL_SEARCH_TOOL_NAME));
            assert!(!first.contains(&"srv__fetch_issue"));
            assert!(tool_names(&captured[1]).contains(&"srv__fetch_issue"));
        });
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
            agent.model = Arc::new(small_context_model(200_000, 8_192));
            agent.auto_compact = enabled;
            agent.context_size = context_size;
            let result = agent.try_auto_compact().await.unwrap();

            assert_eq!(result, expected);
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
            agent.do_compact(None).await.unwrap();
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
        assert_eq!(agent.tool_context().session_id, None);

        let session: SessionRef = "01965087-4c71-7f00-8000-000000000000"
            .parse()
            .expect("valid session id");
        agent.session_id = Some(session.clone());
        assert_eq!(agent.tool_context().session_id, Some(session));
    }
}
