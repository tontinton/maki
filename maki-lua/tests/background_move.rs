//! Moving a running tool call to the background: the receipt answers the
//! dispatcher, the handler runs on, and its final reply reaches the chat.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use maki_agent::cancel::{CancelToken, CancelTrigger};
use maki_agent::tools::test_support::{stub_ctx, stub_ctx_with, stub_seed};
use maki_agent::tools::{ToolContext, ToolExecResult, ToolRegistry};
use maki_agent::{
    AgentEvent, AgentMode, AgentSeed, CallScope, Envelope, EventSender, SessionMailbox,
    ToolDoneEvent, ToolOutput,
};
use maki_lua::{MoveOutcome, PluginHost, UiAction};
use maki_storage::id::{MakiId, SessionRef};
use serde_json::{Value, json};
use test_case::test_case;

const PLUGIN: &str = "background_move_test";
const CALL_ID: &str = "toolu_move";
const MOVABLE: &str = "movable";
const PLAIN: &str = "plain";
const RACING: &str = "racing";
const OUTER: &str = "outer";
const NESTING: &str = "nesting";
const CHILD: &str = "child";
const CHILD_REPLY: &str = "child done";
/// Never loaded: unloading it only runs the reload drain.
const BYSTANDER_PLUGIN: &str = "bystander";
const RECEIPT: &str = "Running in the background";
const FINAL: &str = "done";
/// `FINAL` plus what `ctx:cancelled()` said when the handler ended.
const FINAL_REPLY: &str = "done:false";
const PLAIN_REPLY: &str = "plain done";
const RACING_REPLY: &str = "finished first";
const INNER_REPLY: &str = "inner done";
const DECLINE_REASON: &str = "busy with an edit";
const WAIT: Duration = Duration::from_secs(10);
/// Past the nested call's 1 s `timeout`, so the Rust backstop would fire.
const INNER_SLEEP_MS: u64 = 1500;
const INNER_TIMEOUT_SECS: u64 = 1;

// Mirrors of the runtime's wording.
const ALREADY_MOVED: &str = "already moved";
const MOVED_BY_USER: &str = "Moved to the background by the user";
const MOVED_AT_TIMEOUT: &str = "Hit its 0 s timeout and was moved to the background";
const DECLINED: &str = "movable declined to move";
const TIMED_OUT: &str = "timeout";

const SRC: &str = r#"
local parked, released = {}, {}
local function park(name)
  if released[name] then
    released[name] = nil
    return
  end
  maki.async.await(1, function(cb) parked[name] = cb end)
end
local function release(name)
  local cb = parked[name]
  parked[name] = nil
  if cb then cb() else released[name] = true end
end
local schema = {
  type = "object",
  properties = {
    name = { type = "string" },
    receipt = { type = "string" },
    final = { type = "string" },
    decline = { type = "boolean" },
    reason = { type = "string" },
    deadline = { type = "integer" },
    sleep_ms = { type = "integer" },
    timeout = { type = "integer" },
  },
}
local function tool(name, handler)
  maki.api.register_tool({
    name = name,
    description = name,
    schema = schema,
    audiences = { "main" },
    handler = handler,
  })
end

tool("release", function(input)
  release(input.name)
  return "released"
end)

tool("movable", function(input, ctx)
  local buf = maki.ui.buf()
  buf:on("click", function() release("movable") end)
  ctx:live_buf(buf)
  ctx:on_background(function()
    if input.decline then
      return nil, input.reason
    end
    return input.receipt
  end)
  if input.deadline then
    ctx:set_deadline(input.deadline, { expire = "background" })
  end
  park("movable")
  return input.final .. ":" .. tostring(ctx:cancelled())
end)

tool("plain", function(_, ctx)
  ctx:live_buf(maki.ui.buf())
  park("plain")
  return "plain done"
end)

tool("racing", function(_, ctx)
  local finished = false
  ctx:on_background(function()
    release("racing")
    while not finished do
      maki.async.sleep(0)
    end
    return "too late"
  end)
  park("racing")
  finished = true
  return "finished first"
end)

tool("inner", function(input, ctx)
  ctx:set_deadline(0)
  maki.async.sleep(input.sleep_ms)
  return "inner done"
end)

tool("child", function(_, ctx)
  ctx:live_buf(maki.ui.buf())
  park("child")
  return "child done"
end)

-- Movable only once its child runs, so a move finds the child in flight.
tool("nesting", function(_, ctx)
  local out, err = maki.agent.call_tool(ctx, "child", { name = "child" }, {
    on_live_buf = function()
      ctx:on_background(function() return "nesting moved" end)
    end,
  })
  return out or ("child failed: " .. tostring(err))
end)

tool("outer", function(input, ctx)
  ctx:on_background(function() return "outer moved" end)
  park("outer")
  local out, err = maki.agent.call_tool(ctx, "inner", { sleep_ms = input.sleep_ms }, {
    timeout = input.timeout,
  })
  return out or ("inner failed: " .. tostring(err))
end)
"#;

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

    fn release(&self, name: &str) {
        let (out, failed) = text(exec(
            &self.reg,
            "release",
            json!({ "name": name }),
            &stub_ctx(&AgentMode::Build),
        ));
        assert!(!failed, "{out}");
    }

    /// The final reply of a moved call, as its task tab gets it.
    fn task_done(&self) -> ToolDoneEvent {
        loop {
            let action = self.ui.recv_timeout(WAIT).expect("task tab event");
            if let UiAction::PluginTaskEvent { envelope, .. } = action
                && let AgentEvent::ToolDone(done) = envelope.event
            {
                return *done;
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
}

impl Running {
    /// A model call of {tool} on its own thread, as dispatch makes it: one
    /// scope under the run token, in the chat {chat}.
    fn start(host: &Host, chat: MakiId, tool: &str, input: Value) -> Self {
        let (tx, events) = flume::unbounded();
        let (run, run_token) = CancelToken::new();
        let ctx = call_ctx(&host.reg, chat, &run_token, &EventSender::new(tx, 0));
        let reg = Arc::clone(&host.reg);
        let tool = tool.to_owned();
        let call = thread::spawn(move || exec(&reg, &tool, input, &ctx));
        Self { events, call, run }
    }

    fn wait_for(&self, matches: impl Fn(&AgentEvent) -> bool) {
        loop {
            let envelope = self.events.recv_timeout(WAIT).expect("tool event");
            if matches(&envelope.event) {
                return;
            }
        }
    }

    fn wait_movable(&self) {
        self.wait_for(
            |event| matches!(event, AgentEvent::ToolMovable { id, movable: true } if id == CALL_ID),
        );
    }

    fn wait_live_buf(&self) {
        self.wait_for(|event| matches!(event, AgentEvent::LiveToolBuf { id, .. } if id == CALL_ID));
    }

    fn result(self) -> ToolExecResult {
        self.call.join().expect("call thread")
    }
}

fn call_ctx(
    reg: &Arc<ToolRegistry>,
    chat: MakiId,
    run: &CancelToken,
    events: &EventSender,
) -> ToolContext {
    let mut ctx = stub_ctx_with(&AgentMode::Build, Some(events), Some(CALL_ID));
    let scope = Arc::new(CallScope::new(CALL_ID.to_owned(), run));
    ctx.cancel = scope.cancel().clone();
    ctx.call = Some(scope);
    ctx.session_id = Some(SessionRef::from(chat));
    ctx.registry = Arc::clone(reg);
    ctx
}

fn exec(reg: &ToolRegistry, name: &str, input: Value, ctx: &ToolContext) -> ToolExecResult {
    let inv = reg.get(name).unwrap().tool.parse(&input).unwrap();
    smol::block_on(inv.execute(ctx))
}

fn text(result: ToolExecResult) -> (String, bool) {
    match result.output {
        Ok(ToolOutput::Plain(s) | ToolOutput::Markdown(s)) => (s.text, false),
        Ok(other) => panic!("unexpected output {other:?}"),
        Err(error) => (error, true),
    }
}

fn done_text(done: &ToolDoneEvent) -> (String, bool) {
    (done.output.as_text(), done.is_error)
}

fn movable_input() -> Value {
    json!({ "receipt": RECEIPT, "final": FINAL })
}

/// Moves a running `movable` call by the user and checks its receipt.
fn moved(host: &Host, chat: &Chat) -> Running {
    let running = Running::start(host, chat.id, MOVABLE, movable_input());
    running.wait_movable();
    assert_eq!(host.request(), MoveOutcome::Moved);
    running
}

#[test]
fn a_receipt_answers_the_dispatcher_and_the_final_reply_reaches_the_chat_once() {
    let host = Host::new();
    let chat = Chat::live();
    let result = moved(&host, &chat).result();

    assert!(result.backgrounded);
    let (receipt, failed) = text(result);
    assert!(!failed, "{receipt}");
    assert!(receipt.starts_with(RECEIPT), "{receipt}");
    assert!(receipt.contains(MOVED_BY_USER), "{receipt}");
    assert!(receipt.contains(CALL_ID), "{receipt}");

    host.release(MOVABLE);
    assert_eq!(
        done_text(&host.task_done()),
        (FINAL_REPLY.to_owned(), false)
    );
    assert_eq!(chat.mailbox.drain().len(), 1);
}

#[test]
fn a_second_request_finds_the_call_already_moved() {
    let host = Host::new();
    let chat = Chat::live();
    let running = moved(&host, &chat);

    assert_eq!(
        host.request(),
        MoveOutcome::NotMovable(ALREADY_MOVED.to_owned())
    );

    assert!(running.result().backgrounded);
    host.release(MOVABLE);
    host.task_done();
}

#[test]
fn the_run_token_firing_after_a_move_reaches_nothing() {
    let host = Host::new();
    let chat = Chat::live();
    let running = moved(&host, &chat);
    let run = running.run;
    assert!(running.call.join().expect("call thread").backgrounded);

    run.cancel();
    host.release(MOVABLE);

    assert_eq!(
        done_text(&host.task_done()),
        (FINAL_REPLY.to_owned(), false)
    );
}

#[test]
fn clicks_still_reach_a_moved_call() {
    let host = Host::new();
    let chat = Chat::live();
    assert!(moved(&host, &chat).result().backgrounded);

    host.host
        .event_handle()
        .request_click(CALL_ID.to_owned(), 0);

    assert_eq!(
        done_text(&host.task_done()),
        (FINAL_REPLY.to_owned(), false)
    );
}

#[test_case(Some(DECLINE_REASON), DECLINE_REASON ; "with_a_reason")]
#[test_case(None, DECLINED ; "without_a_reason")]
fn a_declining_hook_keeps_the_call_in_the_foreground(reason: Option<&str>, expected: &str) {
    let host = Host::new();
    let chat = Chat::live();
    let mut input = movable_input();
    input["decline"] = json!(true);
    if let Some(reason) = reason {
        input["reason"] = json!(reason);
    }
    let running = Running::start(&host, chat.id, MOVABLE, input);
    running.wait_movable();

    assert_eq!(host.request(), MoveOutcome::NotMovable(expected.to_owned()));

    host.release(MOVABLE);
    let result = running.result();
    assert!(!result.backgrounded);
    assert_eq!(text(result), (FINAL_REPLY.to_owned(), false));
    assert!(chat.mailbox.drain().is_empty());
}

#[test]
fn a_call_without_a_hook_has_nothing_to_move() {
    let host = Host::new();
    let chat = Chat::live();
    let running = Running::start(&host, chat.id, PLAIN, json!({}));
    running.wait_live_buf();

    assert_eq!(host.request(), MoveOutcome::NoHook);

    host.release(PLAIN);
    assert_eq!(text(running.result()), (PLAIN_REPLY.to_owned(), false));
}

/// The hook lets the handler finish before it hands over its receipt.
#[test]
fn a_finish_that_beats_the_move_is_the_only_answer() {
    let host = Host::new();
    let chat = Chat::live();
    let running = Running::start(&host, chat.id, RACING, json!({}));
    running.wait_movable();

    assert_eq!(host.request(), MoveOutcome::NoHook);

    let result = running.result();
    assert!(!result.backgrounded);
    assert_eq!(text(result), (RACING_REPLY.to_owned(), false));
    assert!(chat.mailbox.drain().is_empty());
}

/// The nested call starts after the move: its 0 s cell deadline and its 1 s
/// Rust backstop both lapse while it sleeps, and neither may end it.
#[test]
fn a_nested_call_outlives_its_deadlines_once_the_scope_moved() {
    let host = Host::new();
    let chat = Chat::live();
    let running = Running::start(
        &host,
        chat.id,
        OUTER,
        json!({ "sleep_ms": INNER_SLEEP_MS, "timeout": INNER_TIMEOUT_SECS }),
    );
    running.wait_movable();
    assert_eq!(host.request(), MoveOutcome::Moved);
    assert!(running.result().backgrounded);

    host.release(OUTER);

    assert_eq!(
        done_text(&host.task_done()),
        (INNER_REPLY.to_owned(), false)
    );
}

/// `/reload` drains in-flight calls first. A child parked under a moved call
/// may run for hours and must not hold it.
#[test]
fn a_reload_passes_a_moved_call_with_a_parked_nested_child() {
    let host = Host::new();
    let chat = Chat::live();
    let running = Running::start(&host, chat.id, NESTING, json!({}));
    running.wait_movable();
    assert_eq!(host.request(), MoveOutcome::Moved);
    assert!(running.result().backgrounded);

    let (unloaded_tx, unloaded_rx) = flume::bounded(1);
    let reloading = thread::spawn(move || {
        let _ = unloaded_tx.send(host.host.unload(BYSTANDER_PLUGIN).is_ok());
        host
    });
    assert_eq!(
        unloaded_rx.recv_timeout(WAIT),
        Ok(true),
        "the reload drain waited on the moved call's child"
    );
    let host = reloading.join().expect("reload thread");

    host.release(CHILD);
    assert_eq!(
        done_text(&host.task_done()),
        (CHILD_REPLY.to_owned(), false)
    );
}

#[test]
fn a_background_deadline_moves_the_call_with_a_live_chat() {
    let host = Host::new();
    let chat = Chat::live();
    let mut input = movable_input();
    input["deadline"] = json!(0);
    let result = Running::start(&host, chat.id, MOVABLE, input).result();

    assert!(result.backgrounded);
    let (receipt, failed) = text(result);
    assert!(!failed, "{receipt}");
    assert!(receipt.starts_with(RECEIPT), "{receipt}");
    assert!(receipt.contains(MOVED_AT_TIMEOUT), "{receipt}");

    host.release(MOVABLE);
    assert_eq!(
        done_text(&host.task_done()),
        (FINAL_REPLY.to_owned(), false)
    );
}

/// Headless, SDK and ACP runs have no chat to deliver to.
#[test]
fn a_background_deadline_times_out_without_a_live_chat() {
    let host = Host::new();
    let mut input = movable_input();
    input["deadline"] = json!(0);
    let result = Running::start(&host, MakiId::generate(), MOVABLE, input).result();

    assert!(!result.backgrounded);
    let (error, failed) = text(result);
    assert!(failed, "{error}");
    assert!(error.contains(TIMED_OUT), "{error}");
}
