use std::sync::Arc;

use serde_json::Value;
use tracing::{error, info, warn};

use maki_providers::provider::Provider;
use maki_providers::{Message, Model, StopReason, StreamResponse, ThinkingConfig, TokenUsage};

use super::compaction::{self, CONTINUE_AFTER_COMPACT};
use super::history::{History, sanitize_cancelled_history};
use super::instructions::LoadedInstructions;
use super::streaming::stream_with_retry;
use super::tool_dispatch::{self, RecentCalls};
use crate::cancel::CancelToken;
use crate::mcp::McpHandle;
use crate::permissions::PermissionManager;
use crate::skill::Skill;
use crate::tools::{Deadline, FileReadTracker, ToolContext};
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, EventSender, ExtractedCommand,
    InterruptSource, TurnCompleteEvent,
};

const MAX_REAUTH_ATTEMPTS: u32 = 2;

enum TurnOutcome {
    Continue,
    Done(Option<StopReason>),
}

pub struct RunOutcome {
    pub history: History,
    pub result: Result<(), AgentError>,
}

pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub skills: Arc<[Skill]>,
    pub config: AgentConfig,
    pub permissions: Arc<PermissionManager>,
    pub session_id: Option<String>,
    pub timeouts: maki_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
}

pub struct AgentRunParams {
    pub history: History,
    pub system: String,
    pub event_tx: EventSender,
    pub tools: Value,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    model: Arc<Model>,
    history: History,
    system: String,
    event_tx: EventSender,
    tools: Value,
    skills: Arc<[Skill]>,
    mode: AgentMode,
    user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
    total_usage: TokenUsage,
    num_turns: u32,
    recent_calls: RecentCalls,
    auto_compact: bool,
    loaded_instructions: LoadedInstructions,
    rollback_len: usize,
    mcp: Option<McpHandle>,
    config: AgentConfig,
    reauth_attempts: u32,
    permissions: Arc<PermissionManager>,
    thinking: ThinkingConfig,
    session_id: Option<String>,
    timeouts: maki_providers::Timeouts,
    file_tracker: Arc<FileReadTracker>,
}

impl Agent {
    pub fn new(params: AgentParams, run: AgentRunParams) -> Self {
        Self {
            provider: params.provider,
            model: Arc::new(params.model),
            skills: params.skills,
            config: params.config,
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
            num_turns: 0,
            recent_calls: RecentCalls::new(),
            auto_compact: compaction::auto_compact_enabled(),
            loaded_instructions: LoadedInstructions::new(),
            rollback_len: 0,
            mcp: None,
            reauth_attempts: 0,
            thinking: ThinkingConfig::Off,
            session_id: params.session_id,
            file_tracker: params.file_tracker,
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

    pub async fn run(mut self, input: AgentInput) -> RunOutcome {
        self.rollback_len = self.history.len();
        let msg = Message::user_with_images(input.message.clone(), input.images);
        self.history.push(msg);
        self.mode = input.mode;
        self.thinking = input.thinking;

        info!(
            model = %self.model.id,
            mode = ?self.mode,
            message_len = input.message.len(),
            "agent run started"
        );

        let result = self.run_loop().await;

        if matches!(result, Err(AgentError::Cancelled)) {
            sanitize_cancelled_history(&mut self.history, self.rollback_len);
        }

        RunOutcome {
            history: self.history,
            result,
        }
    }

    async fn run_loop(&mut self) -> Result<(), AgentError> {
        loop {
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
            self.thinking,
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

        if has_tools {
            self.process_tool_calls(response).await?;
        } else {
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

        if self.try_auto_compact(&usage).await? || self.handle_queued_command().await? {
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
        let ctx = self.tool_context();
        tool_dispatch::process_tool_calls(
            response,
            &mut self.recent_calls,
            self.mcp.as_ref(),
            &mut self.history,
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
            skills: Arc::clone(&self.skills),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
            deadline: Deadline::None,
            config: self.config.clone(),
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            file_tracker: Arc::clone(&self.file_tracker),
        }
    }

    async fn try_auto_compact(&mut self, usage: &TokenUsage) -> Result<bool, AgentError> {
        if !self.auto_compact
            || !compaction::is_overflow(usage, &self.model, self.config.compaction_buffer)
        {
            return Ok(false);
        }
        info!(total_input = usage.total_input(), "auto-compacting");
        self.event_tx.send(AgentEvent::AutoCompacting)?;
        self.do_compact().await?;
        Ok(true)
    }

    async fn do_compact(&mut self) -> Result<(), AgentError> {
        self.total_usage += compaction::compact_history(
            &*self.provider,
            &self.model,
            &mut self.history,
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use maki_providers::provider::{BoxFuture, Provider};
    use maki_providers::{
        ContentBlock, Message, Model, ProviderEvent, Role, StopReason, StreamResponse,
        ThinkingConfig, TokenUsage,
    };
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::Envelope;
    use crate::permissions::PermissionManager;
    use crate::skill::Skill;

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
            _: ThinkingConfig,
            _: Option<&str>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                Ok(responses.remove(0))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
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

    fn make_agent(provider: MockProvider, history: History) -> (Agent, flume::Receiver<Envelope>) {
        let (raw_tx, event_rx) = flume::unbounded();
        let agent = Agent::new(
            AgentParams {
                provider: Arc::new(provider),
                model: default_model(),
                skills: Arc::from([]) as Arc<[Skill]>,
                config: AgentConfig::default(),
                permissions: Arc::new(PermissionManager::new(
                    maki_config::PermissionsConfig {
                        allow_all: true,
                        rules: vec![],
                    },
                    std::path::PathBuf::from("/tmp"),
                )),
                session_id: None,
                timeouts: maki_providers::Timeouts::default(),
                file_tracker: FileReadTracker::fresh(),
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
        let (agent, event_rx) = make_agent(provider, History::new(Vec::new()));
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

            let (agent, event_rx) =
                make_agent(MockProvider::new(responses), History::new(Vec::new()));
            let agent = match source {
                Some(s) => agent.with_interrupt_source(s),
                None => agent,
            };
            let outcome = agent.run(default_input()).await;
            let events = drain_events(&event_rx);

            assert_eq!(
                has_event(&events, |e| matches!(
                    e,
                    AgentEvent::QueueItemConsumed { .. }
                )),
                expect_consumed,
            );
            assert_eq!(
                has_interrupt_in_history(outcome.history.as_slice()),
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

            let (agent, _event_rx) = make_agent(MockProvider::new(responses), History::new(prior));
            let outcome = agent
                .with_interrupt_source(source)
                .run(default_input())
                .await;

            assert!(outcome.result.is_ok());
        });
    }

    #[test_case(true,  900, true  ; "enabled_and_over_threshold")]
    #[test_case(true,  100, false ; "enabled_but_below_threshold")]
    #[test_case(false, 900, false ; "disabled_even_over_threshold")]
    fn try_auto_compact_behavior(enabled: bool, total_input: u32, expected: bool) {
        smol::block_on(async {
            let responses = if expected {
                vec![text_response(StopReason::EndTurn)]
            } else {
                vec![]
            };
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(responses),
                History::new(vec![Message::user("go".into())]),
            );
            agent.model = Arc::new(small_context_model(1000, 200));
            agent.auto_compact = enabled;

            let usage = TokenUsage {
                input: total_input,
                ..Default::default()
            };
            let result = agent.try_auto_compact(&usage).await.unwrap();

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
                    _: ThinkingConfig,
                    _: Option<&'a str>,
                ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
                    Box::pin(async {
                        futures_lite::future::pending::<()>().await;
                        unreachable!()
                    })
                }
                fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
                    Box::pin(async { unimplemented!() })
                }
            }

            let (trigger, cancel) = CancelToken::new();
            trigger.cancel();

            let (raw_tx, _rx) = flume::unbounded();
            let agent = Agent::new(
                AgentParams {
                    provider: Arc::new(HangingProvider),
                    model: default_model(),
                    skills: Arc::from([]) as Arc<[Skill]>,
                    config: AgentConfig::default(),
                    permissions: Arc::new(PermissionManager::new(
                        maki_config::PermissionsConfig {
                            allow_all: true,
                            rules: vec![],
                        },
                        std::path::PathBuf::from("/tmp"),
                    )),
                    session_id: None,
                    timeouts: maki_providers::Timeouts::default(),
                    file_tracker: FileReadTracker::fresh(),
                },
                AgentRunParams {
                    history: History::new(Vec::new()),
                    system: "system".into(),
                    event_tx: EventSender::new(raw_tx, 0),
                    tools: serde_json::json!([]),
                },
            )
            .with_cancel(cancel);

            let outcome = agent.run(default_input()).await;
            assert!(matches!(outcome.result, Err(AgentError::Cancelled)));
            assert_ends_with_cancel_marker(&outcome.history);
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
            let (agent, event_rx) =
                make_agent(MockProvider::new(responses), History::new(Vec::new()));
            let _ = agent.run(default_input()).await;
            let events = drain_events(&event_rx);

            assert!(has_event(&events, |e| matches!(
                e,
                AgentEvent::ToolDone(done) if done.is_error && done.id == expected_error_id
            )));
        });
    }
}
