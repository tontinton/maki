//! Fixtures shared by the integration tests: a live chat, model calls through
//! dispatch, the task tabs a moved call reports to, and a parked provider.

#![allow(dead_code, reason = "each test binary uses a different part")]

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use maki_agent::agent::tool_dispatch;
use maki_agent::tools::test_support::{stub_ctx_with, stub_seed};
use maki_agent::tools::{CallOrigin, ToolContext, ToolRegistry};
use maki_agent::{
    AgentEvent, AgentMode, AgentSeed, CallScope, CancelToken, Envelope, EventSender,
    SessionMailbox, SharedBuf, ToolDoneEvent,
};
use maki_lua::test_support::{runtime, wait_next, wait_reply};
use maki_lua::{EventHandle, MoveOutcome, PluginHost, SessionEndReason, UiAction};
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::{
    AgentError, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, TokenUsage,
};
use maki_storage::id::{MakiId, SessionRef};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Generous: every wait ends on an event or a condition, never on the clock,
/// so only an already failing test pays this.
pub const WAIT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const GATE_FILE: &str = "go";
const GATE_POLL_SECS: &str = "0.02";

/// A chat session the UI is running: its mailbox takes background notices.
pub struct Chat {
    pub id: MakiId,
    pub mailbox: SessionMailbox,
    pub seed: Arc<AgentSeed>,
}

impl Chat {
    pub fn live() -> Self {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let seed = mailbox.publish_seed(stub_seed(id));
        Self { id, mailbox, seed }
    }
}

/// A model call as dispatch makes it: one scope under the run token, in the
/// chat {chat}. The receiver keeps the call's live events flowing.
pub fn call_ctx(
    reg: &Arc<ToolRegistry>,
    chat: MakiId,
    run: &CancelToken,
    call_id: &str,
) -> (ToolContext, UnboundedReceiver<Envelope>) {
    let (tx, events) = unbounded_channel();
    let mut ctx = stub_ctx_with(
        &AgentMode::Build,
        Some(&EventSender::new(tx, 0)),
        Some(call_id),
    );
    let scope = Arc::new(CallScope::new(call_id.to_owned(), run));
    ctx.cancel = scope.cancel().clone();
    ctx.call = Some(scope);
    ctx.session_id = Some(SessionRef::from(chat));
    ctx.mailbox = SessionMailbox::open(chat).ok();
    ctx.registry = Arc::clone(reg);
    (ctx, events)
}

/// Through dispatch, the one path that lands a moved call's final reply.
pub fn exec(reg: &Arc<ToolRegistry>, name: &str, input: Value, ctx: &ToolContext) -> ToolDoneEvent {
    let ctx = ToolContext {
        registry: Arc::clone(reg),
        ..ctx.clone()
    };
    let id = ctx.tool_use_id.clone().unwrap_or_default();
    runtime().block_on(tool_dispatch::run(
        id,
        name,
        &input,
        &ctx,
        CallOrigin::Model,
    ))
}

pub fn done_text(done: &ToolDoneEvent) -> (String, bool) {
    (done.output.as_text(), done.is_error)
}

pub fn done_result(done: &ToolDoneEvent) -> Result<String, String> {
    let text = done.output.as_text();
    if done.is_error { Err(text) } else { Ok(text) }
}

/// The first event of a call that {pick} takes.
pub fn next_event<T>(
    events: &mut UnboundedReceiver<Envelope>,
    mut pick: impl FnMut(AgentEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        let envelope = wait_next(events, deadline.saturating_duration_since(Instant::now()))
            .expect("tool event");
        if let Some(found) = pick(envelope.event) {
            return found;
        }
    }
}

pub fn wait_movable(events: &mut UnboundedReceiver<Envelope>, call_id: &str) {
    next_event(events, |event| {
        matches!(&event, AgentEvent::ToolMovable { id, movable: true } if id == call_id)
            .then_some(())
    });
}

pub fn wait_live_buf(events: &mut UnboundedReceiver<Envelope>, call_id: &str) -> Arc<SharedBuf> {
    next_event(events, |event| match event {
        AgentEvent::LiveToolBuf { id, body } if id == call_id => Some(body),
        _ => None,
    })
}

/// What Ctrl+B on the call's row gets back.
pub fn request_move(handle: &EventHandle, call_id: &str) -> MoveOutcome {
    wait_reply(handle.request_background(call_id.to_owned()), WAIT).expect("move outcome")
}

/// The next event a task tab was sent.
pub fn next_task_event(ui: &mut UnboundedReceiver<UiAction>) -> Envelope {
    loop {
        if let UiAction::PluginTaskEvent { envelope, .. } =
            wait_next(ui, WAIT).expect("task tab event")
        {
            return *envelope;
        }
    }
}

/// The next final reply a task tab got, of a moved call or of a subagent's.
pub fn next_task_done(ui: &mut UnboundedReceiver<UiAction>) -> ToolDoneEvent {
    loop {
        if let AgentEvent::ToolDone(done) = next_task_event(ui).event {
            return *done;
        }
    }
}

pub fn end_session(host: &PluginHost, session: MakiId, reason: SessionEndReason) {
    runtime().block_on(host.event_handle().end_sessions([session], reason));
}

pub fn poll_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(got) = check() {
            return got;
        }
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(POLL_INTERVAL);
    }
}

/// Holds a shell command until released, and lets it finish whatever happens
/// to the test.
pub struct Gate(TempDir);

impl Gate {
    pub fn closed() -> Self {
        Self(TempDir::new().unwrap())
    }

    /// Runs `before`, then waits for the release, then runs `after`.
    pub fn command(&self, before: &str, after: &str) -> String {
        format!(
            "{before}; while [ ! -e '{}' ]; do sleep {GATE_POLL_SECS}; done; {after}",
            self.file().display()
        )
    }

    pub fn release(&self) {
        fs::write(self.file(), "").unwrap();
    }

    fn file(&self) -> PathBuf {
        self.0.path().join(GATE_FILE)
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        fs::write(self.file(), "").ok();
    }
}

/// Answers once released, with `answer` or, on the first turn, with
/// `first_turn` when set. The test holds `release` for as long as the
/// subagent must stay mid-turn.
struct ParkedProvider {
    started: Sender<()>,
    user_turns: Sender<Vec<String>>,
    release: AsyncMutex<UnboundedReceiver<()>>,
    answer: &'static str,
    first_turn: Mutex<Option<ContentBlock>>,
}

impl Provider for ParkedProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        messages: &'a [Message],
        _: &'a str,
        _: &'a Value,
        _: &'a UnboundedSender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let _ = self.user_turns.send(closing_user_texts(messages));
            let _ = self.started.send(());
            let _ = self.release.lock().await.recv().await;
            let first_turn = self.first_turn.lock().unwrap().take();
            let (block, stop_reason) = match first_turn {
                Some(block) => (block, StopReason::ToolUse),
                None => (
                    ContentBlock::Text {
                        text: self.answer.into(),
                    },
                    StopReason::EndTurn,
                ),
            };
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![block],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(stop_reason),
                input_transformations: Vec::new(),
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// The texts of the user messages that close {messages}, oldest first.
fn closing_user_texts(messages: &[Message]) -> Vec<String> {
    let mut texts: Vec<String> = messages
        .iter()
        .rev()
        .take_while(|m| matches!(m.role, Role::User))
        .filter_map(|m| m.first_text_content().map(str::to_owned))
        .collect();
    texts.reverse();
    texts
}

pub struct Parked {
    pub provider: Arc<dyn Provider>,
    pub started: Receiver<()>,
    /// What each request's closing user turn says.
    pub user_turns: Receiver<Vec<String>>,
    pub release: UnboundedSender<()>,
}

impl Parked {
    pub fn new(answer: &'static str) -> Self {
        Self::with_first_turn(answer, None)
    }

    /// `first_turn` is a tool call the first turn makes instead of answering.
    pub fn with_first_turn(answer: &'static str, first_turn: Option<ContentBlock>) -> Self {
        let (started_tx, started) = channel();
        let (release, release_rx) = unbounded_channel();
        let (user_turns_tx, user_turns) = channel();
        Self {
            provider: Arc::new(ParkedProvider {
                started: started_tx,
                user_turns: user_turns_tx,
                release: AsyncMutex::new(release_rx),
                answer,
                first_turn: Mutex::new(first_turn),
            }),
            started,
            user_turns,
            release,
        }
    }

    pub fn wait_started(&self) {
        self.started.recv_timeout(WAIT).expect("subagent turn");
    }
}
