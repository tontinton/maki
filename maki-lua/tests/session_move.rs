//! Subagent sessions under a call that moved to the background: they report
//! to the chat, outlive the run that started them, and stop by their own id.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use maki_agent::cancel::{CancelToken, CancelTrigger};
use maki_agent::tools::test_support::{stub_ctx, stub_ctx_with, stub_seed};
use maki_agent::tools::{ToolContext, ToolExecResult, ToolRegistry};
use maki_agent::{
    AgentEvent, AgentMode, AgentSeed, CallScope, CancelMap, Envelope, EventSender, SessionMailbox,
    ToolDoneEvent, ToolOutput,
};
use maki_lua::{MoveOutcome, PluginHost, UiAction};
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::{
    AgentError, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, TokenUsage,
};
use maki_storage::id::{MakiId, SessionRef};
use serde_json::{Value, json};
use test_case::test_case;

const PLUGIN: &str = "session_move_test";
const CALL_ID: &str = "toolu_session_move";
const SUB: &str = "sub";
const OUTER: &str = "outer";
const OPENED: &str = "opened";
const RESULT: &str = "subagent answer";
const ANSWER: &str = "answer: subagent answer";
const PROMPT_CANCELLED: &str = "prompt failed: cancelled";
const WAIT: Duration = Duration::from_secs(10);

const SRC: &str = r#"
local opened = {}
local function tool(name, handler)
  maki.api.register_tool({
    name = name,
    description = name,
    schema = { type = "object", properties = {} },
    audiences = { "main" },
    handler = handler,
  })
end

local function subagent(ctx)
  local sess, err = maki.agent.session(ctx, {})
  if not sess then
    return "open failed: " .. err
  end
  opened[#opened + 1] = sess:id()
  local result, prompt_err = sess:prompt("go")
  sess:close()
  if prompt_err then
    return "prompt failed: " .. prompt_err
  end
  return "answer: " .. result.text
end

tool("sub", function(_, ctx)
  ctx:on_background(function() return "sub moved" end)
  return subagent(ctx)
end)

tool("nested", function(_, ctx)
  return subagent(ctx)
end)

tool("outer", function(_, ctx)
  ctx:on_background(function() return "outer moved" end)
  local out, err = maki.agent.call_tool(ctx, "nested", {})
  return out or ("nested failed: " .. tostring(err))
end)

tool("opened", function()
  return table.concat(opened, ",")
end)
"#;

/// Answers once released. The test holds `release` for as long as the
/// subagent must stay mid-turn.
struct ParkedProvider {
    started: flume::Sender<()>,
    release: flume::Receiver<()>,
}

impl Provider for ParkedProvider {
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
        Box::pin(async move {
            let _ = self.started.send(());
            let _ = self.release.recv_async().await;
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: RESULT.into(),
                    }],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::EndTurn),
                input_transformations: Vec::new(),
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

struct Parked {
    provider: Arc<dyn Provider>,
    started: flume::Receiver<()>,
    release: flume::Sender<()>,
}

impl Parked {
    fn new() -> Self {
        let (started_tx, started) = flume::unbounded();
        let (release, release_rx) = flume::unbounded();
        Self {
            provider: Arc::new(ParkedProvider {
                started: started_tx,
                release: release_rx,
            }),
            started,
            release,
        }
    }

    fn wait_started(&self) {
        self.started.recv_timeout(WAIT).expect("subagent turn");
    }
}

struct Host {
    reg: Arc<ToolRegistry>,
    host: PluginHost,
    ui: flume::Receiver<UiAction>,
}

impl Host {
    fn new() -> Self {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
        host.load_source(PLUGIN, SRC).unwrap();
        host.ui_attachment().attach();
        let ui = host.ui_action_rx();
        Self { reg, host, ui }
    }

    fn request(&self) -> MoveOutcome {
        self.host
            .event_handle()
            .request_background(CALL_ID.to_owned())
            .recv_timeout(WAIT)
            .expect("move outcome")
    }

    /// The `ui_id` of every session the plugin opened so far.
    fn opened(&self) -> String {
        let (out, failed) = text(exec(&self.reg, OPENED, &stub_ctx(&AgentMode::Build)));
        assert!(!failed, "{out}");
        out
    }

    /// What the task tabs heard up to the moved call's final reply, and
    /// that reply.
    fn until_task_done(&self) -> (Vec<AgentEvent>, ToolDoneEvent) {
        let mut relayed = Vec::new();
        loop {
            let action = self.ui.recv_timeout(WAIT).expect("task tab event");
            let UiAction::PluginTaskEvent { envelope, .. } = action else {
                continue;
            };
            match (envelope.event, envelope.subagent) {
                (AgentEvent::ToolDone(done), None) if done.id == CALL_ID => {
                    return (relayed, *done);
                }
                (event, _) => relayed.push(event),
            }
        }
    }
}

struct Chat {
    id: MakiId,
    mailbox: SessionMailbox,
    _seed: Arc<AgentSeed>,
}

impl Chat {
    fn live() -> Self {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let seed = mailbox.publish_seed(stub_seed(id));
        Self {
            id,
            mailbox,
            _seed: seed,
        }
    }
}

struct Running {
    events: flume::Receiver<Envelope>,
    call: JoinHandle<ToolExecResult>,
    run: CancelTrigger,
    /// The run's `subagent_cancels`, which run end drains.
    cancels: Arc<CancelMap<String>>,
}

impl Running {
    /// A model call of {tool} on its own thread, as dispatch makes it: one
    /// scope under the run token, in the chat {chat}.
    fn start(host: &Host, chat: MakiId, tool: &str, parked: &Parked) -> Self {
        let (tx, events) = flume::unbounded();
        let (run, run_token) = CancelToken::new();
        let mut ctx = stub_ctx_with(
            &AgentMode::Build,
            Some(&EventSender::new(tx, 0)),
            Some(CALL_ID),
        );
        let scope = Arc::new(CallScope::new(CALL_ID.to_owned(), &run_token));
        ctx.cancel = scope.cancel().clone();
        ctx.call = Some(scope);
        ctx.session_id = Some(SessionRef::from(chat));
        ctx.registry = Arc::clone(&host.reg);
        ctx.provider = Arc::clone(&parked.provider);
        let cancels = Arc::clone(&ctx.subagent_cancels);
        let reg = Arc::clone(&host.reg);
        let tool = tool.to_owned();
        let call = thread::spawn(move || exec(&reg, &tool, &ctx));
        Self {
            events,
            call,
            run,
            cancels,
        }
    }
}

fn exec(reg: &ToolRegistry, name: &str, ctx: &ToolContext) -> ToolExecResult {
    let inv = reg.get(name).unwrap().tool.parse(&json!({})).unwrap();
    smol::block_on(inv.execute(ctx))
}

fn text(result: ToolExecResult) -> (String, bool) {
    match result.output {
        Ok(ToolOutput::Plain(s) | ToolOutput::Markdown(s)) => (s.text, false),
        Ok(other) => panic!("unexpected output {other:?}"),
        Err(error) => (error, true),
    }
}

fn position(events: &[AgentEvent], matches: impl Fn(&AgentEvent) -> bool) -> Option<usize> {
    events.iter().position(matches)
}

/// The run that started the call ends after the move: its token fires and
/// its `subagent_cancels` drain. The subagent finishes its turn anyway, and
/// that turn reaches the chat's tab instead of the parent run, which would
/// drop it as stale.
#[test]
fn a_moved_subagent_outlives_its_run_and_reports_to_the_chat() {
    let host = Host::new();
    let chat = Chat::live();
    let parked = Parked::new();
    let running = Running::start(&host, chat.id, SUB, &parked);
    parked.wait_started();
    assert_eq!(host.request(), MoveOutcome::Moved);
    let Running {
        events,
        call,
        run,
        cancels,
    } = running;
    assert!(call.join().expect("call thread").backgrounded);

    run.cancel();
    cancels.cancel_all();
    parked.release.send(()).unwrap();

    let (relayed, done) = host.until_task_done();
    assert_eq!(
        (done.output.as_text(), done.is_error),
        (ANSWER.to_owned(), false)
    );
    let turn = position(&relayed, |e| matches!(e, AgentEvent::TurnComplete(_)));
    let run_done = position(&relayed, |e| matches!(e, AgentEvent::Done { .. }));
    assert!(
        matches!((turn, run_done), (Some(turn), Some(run_done)) if turn < run_done),
        "{turn:?} {run_done:?}"
    );
    assert!(
        !events
            .try_iter()
            .any(|envelope| matches!(envelope.event, AgentEvent::TurnComplete(_)))
    );
    assert_eq!(chat.mailbox.drain().len(), 1);
}

#[test]
fn an_unmoved_subagent_ends_with_its_run() {
    let host = Host::new();
    let chat = Chat::live();
    let parked = Parked::new();
    let running = Running::start(&host, chat.id, SUB, &parked);
    parked.wait_started();

    running.run.cancel();

    assert!(!running.call.join().expect("call thread").backgrounded);
    assert!(chat.mailbox.drain().is_empty());
}

/// A subagent nested in a moved call already has a chat under the call's row.
/// The move hands that chat to the task tabs at once: one quiet until the
/// turn ends would otherwise be taken for dead.
#[test]
fn a_move_opens_the_tab_of_a_nested_subagent_mid_turn() {
    let host = Host::new();
    let chat = Chat::live();
    let parked = Parked::new();
    let running = Running::start(&host, chat.id, OUTER, &parked);
    parked.wait_started();
    assert_eq!(host.request(), MoveOutcome::Moved);
    assert!(running.call.join().expect("call thread").backgrounded);

    let id = host.opened();
    let opened = loop {
        if let UiAction::PluginTaskOpened {
            session,
            plugin,
            info,
        } = host.ui.recv_timeout(WAIT).expect("tab opened at the move")
        {
            break (session, plugin.to_string(), info.parent_tool_use_id);
        }
    };
    assert_eq!(opened, (chat.id, PLUGIN.to_owned(), id));

    parked.release.send(()).unwrap();
    host.until_task_done();
}

/// Esc in the adopted tab stops the subagent by its own id while its turn
/// is parked, so only the stop can end the call. A subagent nested in a
/// moved call stops alone, and the call finishes with what it got.
#[test_case(SUB, None ; "a_top_level_task")]
#[test_case(OUTER, Some(PROMPT_CANCELLED) ; "a_subagent_nested_in_a_moved_call")]
fn stopping_an_adopted_subagent_by_its_id_stops_it(tool: &str, reply: Option<&str>) {
    let host = Host::new();
    let chat = Chat::live();
    let parked = Parked::new();
    let running = Running::start(&host, chat.id, tool, &parked);
    parked.wait_started();
    assert_eq!(host.request(), MoveOutcome::Moved);
    assert!(running.call.join().expect("call thread").backgrounded);

    let id = host.opened();
    if reply.is_none() {
        assert_eq!(id, CALL_ID);
    }
    host.host.event_handle().cancel_plugin_session(&id);

    let (_, done) = host.until_task_done();
    if let Some(reply) = reply {
        assert_eq!(done.output.as_text(), reply);
    }
}
