use std::sync::Arc;

use serde_json::Value;
use tracing::{error, info, warn};

use maki_providers::provider::Provider;
use maki_providers::{
    ContentBlock, Message, Model, RequestOptions, Role, StopReason, StreamResponse, TokenUsage,
};

use super::compaction::{self, CONTINUE_AFTER_COMPACT};
use super::history::{History, sanitize_cancelled_history};
use super::instructions::LoadedInstructions;
use super::streaming::stream_with_retry;
use super::tool_dispatch::{self, RecentCalls};
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::McpHandle;
use crate::permissions::PermissionManager;
use crate::tools::{Deadline, FileReadTracker, ToolContext};
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, EventSender, ExtractedCommand,
    InterruptSource, TurnCompleteEvent,
};
use maki_config::ToolOutputLines;

const MAX_REAUTH_ATTEMPTS: u32 = 2;
const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";

pub fn resolve_compaction_model(
    provider: &Arc<dyn Provider>,
    model: &Model,
    timeouts: maki_providers::Timeouts,
) -> (Arc<dyn Provider>, Model) {
    if let Some(spec) = maki_providers::model_registry::model_registry()
        .read()
        .unwrap()
        .spec_for_tier_any(maki_providers::ModelTier::Compaction)
        && let Ok(mut m) = Model::from_spec(&spec)
        && let Ok(p) = maki_providers::provider::from_model(&mut m, timeouts)
    {
        return (Arc::from(p), m);
    }
    (Arc::clone(provider), model.clone())
}

enum TurnOutcome {
    Continue,
    Done(Option<StopReason>),
}

pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub session_id: Option<String>,
    pub timeouts: maki_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub subagent_cancels: Arc<CancelMap<String>>,
    pub registry: Arc<crate::tools::ToolRegistry>,
}

pub struct AgentRunParams<'h> {
    pub history: &'h mut History,
    pub system: String,
    pub event_tx: EventSender,
    pub tools: Value,
}

pub struct Agent<'h> {
    provider: Arc<dyn Provider>,
    model: Arc<Model>,
    history: &'h mut History,
    system: String,
    event_tx: EventSender,
    tools: Value,
    mode: AgentMode,
    user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
    total_usage: TokenUsage,
    context_size: u32,
    num_turns: u32,
    recent_calls: RecentCalls,
    auto_compact: bool,
    loaded_instructions: LoadedInstructions,
    rollback_len: usize,
    mcp: Option<McpHandle>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    reauth_attempts: u32,
    post_tool_empty_retried: bool,
    permissions: Arc<PermissionManager>,
    opts: RequestOptions,
    session_id: Option<String>,
    timeouts: maki_providers::Timeouts,
    file_tracker: Arc<FileReadTracker>,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    subagent_cancels: Arc<crate::cancel::CancelMap<String>>,
    registry: Arc<crate::tools::ToolRegistry>,
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
            total_usage: TokenUsage::default(),
            context_size: 0,
            num_turns: 0,
            recent_calls: RecentCalls::new(),
            auto_compact: compaction::auto_compact_enabled(),
            loaded_instructions: LoadedInstructions::new(),
            rollback_len: 0,
            mcp: None,
            reauth_attempts: 0,
            post_tool_empty_retried: false,
            opts: RequestOptions::default(),
            session_id: params.session_id,
            file_tracker: params.file_tracker,
            prompt_slots: params.prompt_slots,
            subagent_cancels: params.subagent_cancels,
            registry: params.registry,
        }
    }

    pub fn with_mcp(mut self, mcp: Option<McpHandle>) -> Self {
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

    pub fn with_loaded_instructions(mut self, loaded: LoadedInstructions) -> Self {
        self.loaded_instructions = loaded;
        self
    }

    pub async fn run(&mut self, input: AgentInput) -> Result<(), AgentError> {
        self.rollback_len = self.history.len();
        let msg = Message::user_with_images(input.message.clone(), input.images);
        self.history.push(msg);
        self.mode = input.mode;
        self.opts = RequestOptions {
            thinking: input.thinking,
            fast: input.fast,
        };

        info!(
            model = %self.model.id,
            mode = ?self.mode,
            message_len = input.message.len(),
            "agent run started"
        );

        let result = self.run_loop().await;

        if matches!(result, Err(AgentError::Cancelled)) {
            sanitize_cancelled_history(self.history, self.rollback_len);
        }

        result
    }

    async fn run_loop(&mut self) -> Result<(), AgentError> {
        loop {
            if let Some(max) = self.config.max_turns
                && self.num_turns >= max
            {
                self.emit_done(None)?;
                return Ok(());
            }
            match self.turn().await? {
                TurnOutcome::Continue => {}
                TurnOutcome::Done(stop_reason) => {
                    self.emit_done(stop_reason)?;
                    return Ok(());
                }
            }
        }
    }

    async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let response = match stream_with_retry(
            &*self.provider,
            &self.model,
            self.history.as_slice(),
            &self.system,
            &self.tools,
            &self.event_tx,
            &self.cancel,
            self.opts,
            self.session_id.as_deref(),
        )
        .await
        {
            Ok(r) => {
                self.reauth_attempts = 0;
                r
            }
            Err(e) if e.is_auth_error() => {
                return self.wait_for_reauth(e).await;
            }
            Err(e) => {
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

        self.emit_turn_complete(&response)?;
        let usage = response.usage;
        self.total_usage += usage;
        self.context_size = usage.total_input();

        if has_tools {
            let history_len_before = self.history.len();
            self.process_tool_calls(response).await?;
            self.context_size +=
                estimate_message_tokens(&self.history.as_slice()[history_len_before..]);
        } else {
            let has_text = response.message.first_text_content().is_some();

            if !has_text && !self.post_tool_empty_retried && self.history.has_recent_tool_results(5)
            {
                self.post_tool_empty_retried = true;
                warn!("empty response after tool calls, nudging model to continue");
                self.event_tx.send(AgentEvent::Nudge)?;
                self.history.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "(empty)".into(),
                    }],
                    ..Default::default()
                });
                self.history.push(Message::synthetic(NUDGE_PROMPT.into()));
                return Ok(TurnOutcome::Continue);
            }

            self.history.push(response.message);

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
            Ok(TurnOutcome::Done(stop_reason))
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
        self.event_tx
            .send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: response.message.clone(),
                usage: response.usage,
                model: self.model.id.clone(),
                context_size: Some(response.usage.context_tokens()),
            })))
    }

    fn emit_done(&self, stop_reason: Option<StopReason>) -> Result<(), AgentError> {
        info!(
            self.num_turns,
            total_input = self.total_usage.input,
            total_output = self.total_usage.output,
            "agent run completed"
        );
        self.event_tx.send(AgentEvent::Done {
            usage: self.total_usage,
            num_turns: self.num_turns,
            stop_reason,
        })
    }

    async fn process_tool_calls(&mut self, response: StreamResponse) -> Result<(), AgentError> {
        self.post_tool_empty_retried = false;
        let ctx = self.tool_context();
        tool_dispatch::process_tool_calls(
            response,
            &mut self.recent_calls,
            self.mcp.as_ref(),
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
            tool_use_id: None,
            user_response_rx: self.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
            deadline: Deadline::None,
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            file_tracker: Arc::clone(&self.file_tracker),
            prompt_slots: Arc::clone(&self.prompt_slots),
            opts: self.opts,
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            registry: Arc::clone(&self.registry),
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
        self.event_tx.send(AgentEvent::AutoCompacting)?;
        self.do_compact().await?;
        Ok(true)
    }

    async fn do_compact(&mut self) -> Result<(), AgentError> {
        let (compact_provider, compact_model) =
            resolve_compaction_model(&self.provider, &self.model, self.timeouts);
        self.total_usage += compaction::compact_history(
            &*compact_provider,
            &compact_model,
            self.history,
            &self.event_tx,
            &self.cancel,
        )
        .await?;
        self.rollback_len = self.history.len();
        self.history
            .push(Message::synthetic(CONTINUE_AFTER_COMPACT.into()));
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
            ExtractedCommand::Interrupt(mut input, _) => {
                self.event_tx.send(AgentEvent::QueueItemConsumed {
                    text: input.message.clone(),
                    image_count: input.images.len(),
                })?;
                for msg in std::mem::take(&mut input.preamble) {
                    self.history.push(msg);
                }
                self.mode = input.mode.clone();
                let display = input.message.clone();
                let wrapped = format!(
                    "<user-interrupt>\nThe user sent a new message while you were working. Address it and continue.\n\n{display}\n</user-interrupt>"
                );
                self.history.push(Message::user_display(wrapped, display));
            }
            ExtractedCommand::Compact(_) => {
                self.do_compact().await?;
            }
        }
        Ok(true)
    }
}

const CHARS_PER_TOKEN: usize = 4;

fn estimate_message_tokens(messages: &[Message]) -> u32 {
    let total_bytes: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.len()),
            ContentBlock::ToolResult { content, .. } => Some(content.len()),
            ContentBlock::ToolUse { input, .. } => Some(input.to_string().len()),
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
    use crate::permissions::PermissionManager;

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
    }

    impl MockProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&str>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                Ok(responses.remove(0))
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
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
        }
    }

    fn make_agent(
        provider: MockProvider,
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
                )),
                session_id: None,
                timeouts: maki_providers::Timeouts::default(),
                file_tracker: FileReadTracker::fresh(),
                prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
                subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
                registry: Arc::new(crate::tools::ToolRegistry::with_natives()),
            },
            AgentRunParams {
                history,
                system: "system".into(),
                event_tx: EventSender::new(raw_tx, 0),
                tools: serde_json::json!([]),
            },
        );
        (agent, event_rx)
    }

    fn default_input() -> AgentInput {
        AgentInput {
            message: "hello".into(),
            mode: AgentMode::Build,
            ..Default::default()
        }
    }

    fn drain_events(rx: &flume::Receiver<Envelope>) -> Vec<Envelope> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    async fn run_agent(provider: MockProvider) -> (u32, Option<StopReason>) {
        let mut history = History::new(Vec::new());
        let (mut agent, event_rx) = make_agent(provider, &mut history);
        let _ = agent.run(default_input()).await;
        drain_events(&event_rx)
            .into_iter()
            .find_map(|e| match e.event {
                AgentEvent::Done {
                    num_turns,
                    stop_reason,
                    ..
                } => Some((num_turns, stop_reason)),
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
                content: vec![ContentBlock::ToolUse {
                    id: tool_id.into(),
                    name: tool_name.into(),
                    input: serde_json::json!({"pattern": "*.nonexistent_test_xyz", "path": "/tmp"}),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
        }
    }

    fn small_context_model(context_window: u32, max_output_tokens: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model.max_output_tokens = max_output_tokens;
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

    #[test_case(&[StopReason::EndTurn],                                                     1, Some(StopReason::EndTurn)  ; "end_turn_completes")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn],                                 2, Some(StopReason::EndTurn)  ; "max_tokens_continues")]
    #[test_case(&[StopReason::MaxTokens, StopReason::MaxTokens, StopReason::MaxTokens, StopReason::MaxTokens], 4, Some(StopReason::MaxTokens) ; "max_tokens_gives_up_after_limit")]
    fn turn_counting(stops: &[StopReason], expected_turns: u32, expected_stop: Option<StopReason>) {
        smol::block_on(async {
            let responses: Vec<_> = stops.iter().map(|s| text_response(*s)).collect();
            let provider = MockProvider::new(responses);
            let (turns, stop_reason) = run_agent(provider).await;
            assert_eq!(turns, expected_turns);
            assert_eq!(stop_reason, expected_stop);
        });
    }

    #[test_case(Some(true),  true,  true  ; "after_tool_use_turn")]
    #[test_case(Some(false), true,  true  ; "after_text_only_turn")]
    #[test_case(None,        false, false ; "channel_empty")]
    fn interrupt_handling(queued: Option<bool>, expect_consumed: bool, expect_injected: bool) {
        smol::block_on(async {
            let source = if queued.is_some() {
                Some(MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                    default_input(),
                    0,
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

    #[test_case(
        (0..10).map(|i| Message::user(format!("msg {i}"))).collect(),
        vec![ExtractedCommand::Compact(0)],
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
                    AgentEvent::AutoCompacting
                )),
                expected,
            );
        });
    }

    #[test]
    fn cancel_token_aborts_during_api_call() {
        smol::block_on(async {
            struct HangingProvider;
            impl Provider for HangingProvider {
                fn stream_message<'a>(
                    &'a self,
                    _: &'a Model,
                    _: &'a [Message],
                    _: &'a str,
                    _: &'a Value,
                    _: &'a flume::Sender<ProviderEvent>,
                    _: RequestOptions,
                    _: Option<&'a str>,
                ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
                    Box::pin(async {
                        futures_lite::future::pending::<()>().await;
                        unreachable!()
                    })
                }
                fn list_models(
                    &self,
                ) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>>
                {
                    Box::pin(async { unimplemented!() })
                }
            }

            let (trigger, cancel) = CancelToken::new();
            trigger.cancel();

            let (raw_tx, _rx) = flume::unbounded();
            let mut history = History::new(Vec::new());
            let mut agent = Agent::new(
                AgentParams {
                    provider: Arc::new(HangingProvider),
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
                    )),
                    session_id: None,
                    timeouts: maki_providers::Timeouts::default(),
                    file_tracker: FileReadTracker::fresh(),
                    prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
                    subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
                    registry: Arc::new(crate::tools::ToolRegistry::with_natives()),
                },
                AgentRunParams {
                    history: &mut history,
                    system: "system".into(),
                    event_tx: EventSender::new(raw_tx, 0),
                    tools: serde_json::json!([]),
                },
            )
            .with_cancel(cancel);

            let result = agent.run(default_input()).await;
            assert!(matches!(result, Err(AgentError::Cancelled)));
            drop(agent);
            assert_ends_with_cancel_marker(&history);
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
        3, true
        ; "nudge_on_empty_after_tools"
    )]
    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            text_response(StopReason::EndTurn),
        ],
        2, false
        ; "no_nudge_when_text_after_tools"
    )]
    #[test_case(
        vec![
            empty_response(),
            text_response(StopReason::EndTurn),
        ],
        1, false
        ; "no_nudge_without_recent_tools"
    )]
    fn nudge_behavior(responses: Vec<StreamResponse>, expected_turns: u32, expect_nudge: bool) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            assert_eq!(
                has_event(&events, |e| matches!(e, AgentEvent::Nudge)),
                expect_nudge,
            );
            let done = events
                .iter()
                .find_map(|e| match &e.event {
                    AgentEvent::Done { num_turns, .. } => Some(*num_turns),
                    _ => None,
                })
                .expect("expected Done event");
            assert_eq!(done, expected_turns);
        });
    }
}
