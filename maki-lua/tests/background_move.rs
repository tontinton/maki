//! Moving a running tool call to the background: the receipt answers the
//! dispatcher, the handler runs on, and its final reply reaches the chat.

mod common;

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use common::{
    Chat, WAIT, call_ctx, done_text, exec, next_task_done, request_move, wait_live_buf,
    wait_movable,
};
use maki_agent::cancel::{CancelToken, CancelTrigger};
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentMode, Envelope, SessionMailbox, ToolDoneEvent};
use maki_lua::{MoveOutcome, PluginHost, UiAction};
use maki_storage::id::MakiId;
use serde_json::{Value, json};
use test_case::test_case;

const PLUGIN: &str = "background_move_test";
const CALL_ID: &str = "toolu_move";
const SUBAGENT_TASK_ID: &str = "toolu_subagent";
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
const REDACTOR_PLUGIN: &str = "redactor";
const REDACTED: &str = "[redacted]";
const REDACTED_RECEIPT: &str = "[redacted receipt]";
const RECEIPT_REFUSER_PLUGIN: &str = "receipt_refuser";
const RECEIPT_REFUSED: &str = "receipts are not allowed here";
/// Past the nested call's 1 s `timeout`, so the Rust backstop would fire.
const INNER_SLEEP_MS: u64 = 1500;
const INNER_TIMEOUT_SECS: u64 = 1;

// Mirrors of the runtime's wording.
const ALREADY_MOVED: &str = "already moved";
const MOVED_BY_USER: &str = "Moved to the background by the user";
const MOVED_AT_TIMEOUT: &str = "Hit its 0 s timeout and was moved to the background";
const DECLINED: &str = "movable declined to move";
const TIMED_OUT: &str = "timed out after";
const RUN_ENDED: &str = "the run this call reports to has ended";
const CANCELLED: &str = "cancelled";
const HOOK: &str = "hook";
/// `MAX_BACKGROUND_TASKS`, which the crate keeps private.
const TASK_CAP: usize = 16;
const CAP_REACHED: &str = "background tasks already running";

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
    park_hook = { type = "boolean" },
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
    if input.park_hook then
      ctx:live_buf(maki.ui.buf())
      park("hook")
    end
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

/// An output layer on `movable` that hides whatever the call said, and
/// tells its receipt apart.
const REDACTOR_SRC: &str = r#"
maki.api.set_slot("tool.movable.output", function(prev, out, ctx)
  out.text = ctx.receipt and "[redacted receipt]" or "[redacted]"
  return prev(out, ctx)
end)
"#;

/// An output layer on `movable` that fails its receipt.
const RECEIPT_REFUSER_SRC: &str = r#"
maki.api.set_slot("tool.movable.output", function(prev, out, ctx)
  if ctx.receipt then
    return nil, "receipts are not allowed here"
  end
  return prev(out, ctx)
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
        request_move(&self.host.event_handle(), CALL_ID)
    }

    fn release(&self, name: &str) {
        let (out, failed) = done_text(&exec(
            &self.reg,
            "release",
            json!({ "name": name }),
            &stub_ctx(&AgentMode::Build),
        ));
        assert!(!failed, "{out}");
    }

    /// The final reply of a moved call, as its task tab gets it.
    fn task_done(&self) -> ToolDoneEvent {
        next_task_done(&self.ui)
    }
}

/// Makes the call one of a subagent's in the chat: the chat's session, but
/// its own run reading its own mailbox.
fn as_subagent(ctx: &mut ToolContext, mailbox: &SessionMailbox) {
    ctx.task_id = Some(Arc::from(SUBAGENT_TASK_ID));
    ctx.mailbox = Some(mailbox.clone());
}

struct Running {
    id: String,
    events: flume::Receiver<Envelope>,
    call: JoinHandle<ToolDoneEvent>,
    run: CancelTrigger,
}

impl Running {
    fn start(host: &Host, chat: MakiId, tool: &str, input: Value) -> Self {
        Self::start_as(host, chat, CALL_ID, tool, input, |_| {})
    }

    /// A model call of {tool} on its own thread, as dispatch makes it: one
    /// scope under the run token, in the chat {chat}.
    fn start_as(
        host: &Host,
        chat: MakiId,
        call_id: &str,
        tool: &str,
        input: Value,
        adjust: impl FnOnce(&mut ToolContext),
    ) -> Self {
        let (run, run_token) = CancelToken::new();
        let (mut ctx, events) = call_ctx(&host.reg, chat, &run_token, call_id);
        adjust(&mut ctx);
        let reg = Arc::clone(&host.reg);
        let tool = tool.to_owned();
        let call = thread::spawn(move || exec(&reg, &tool, input, &ctx));
        Self {
            id: call_id.to_owned(),
            events,
            call,
            run,
        }
    }

    fn wait_movable(&self) {
        wait_movable(&self.events, &self.id);
    }

    fn wait_live_buf(&self) {
        wait_live_buf(&self.events, &self.id);
    }

    /// Ends the run once the call answered, the way a turn ends on its last
    /// reply. A moved call must not notice.
    fn result(self) -> ToolDoneEvent {
        let done = self.call.join().expect("call thread");
        self.run.cancel();
        done
    }
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
    let (receipt, failed) = done_text(&result);
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
    assert_eq!(done_text(&result), (FINAL_REPLY.to_owned(), false));
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
    assert_eq!(
        done_text(&running.result()),
        (PLAIN_REPLY.to_owned(), false)
    );
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
    assert_eq!(done_text(&result), (RACING_REPLY.to_owned(), false));
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
    let (receipt, failed) = done_text(&result);
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
    let (error, failed) = done_text(&result);
    assert!(failed, "{error}");
    assert!(error.contains(TIMED_OUT), "{error}");
}

/// The receipt stays unsettled until a run drains the notice that answers it,
/// so a quit before then keeps it for the session meta.
#[test]
fn a_moved_calls_receipt_is_unsettled_until_its_notice_is_drained() {
    let host = Host::new();
    let chat = Chat::live();
    assert!(moved(&host, &chat).result().backgrounded);

    assert_eq!(chat.mailbox.unsettled(), [CALL_ID]);

    host.release(MOVABLE);
    host.task_done();

    assert_eq!(chat.mailbox.unsettled(), [CALL_ID]);
    assert_eq!(chat.mailbox.claim_wake().len(), 1);
    assert!(chat.mailbox.unsettled().is_empty());
}

#[test]
fn the_user_moves_a_subagents_call_and_it_reports_to_the_subagent() {
    let host = Host::new();
    let chat = Chat::live();
    let subagent = SessionMailbox::register(MakiId::generate());
    let running = Running::start_as(&host, chat.id, CALL_ID, MOVABLE, movable_input(), |ctx| {
        as_subagent(ctx, &subagent)
    });
    running.wait_movable();

    assert_eq!(host.request(), MoveOutcome::Moved);
    assert!(running.result().backgrounded);
    assert_eq!(subagent.unsettled(), [CALL_ID]);

    host.release(MOVABLE);
    assert_eq!(
        done_text(&host.task_done()),
        (FINAL_REPLY.to_owned(), false)
    );
    assert_eq!(subagent.claim_wake().len(), 1);
    assert!(chat.mailbox.unsettled().is_empty());
    assert!(chat.mailbox.drain().is_empty());
}

/// A subagent waits for its moved calls, so it never moves one itself.
#[test]
fn a_subagents_background_deadline_times_out() {
    let host = Host::new();
    let chat = Chat::live();
    let subagent = SessionMailbox::register(MakiId::generate());
    let mut input = movable_input();
    input["deadline"] = json!(0);

    let result = Running::start_as(&host, chat.id, CALL_ID, MOVABLE, input, |ctx| {
        as_subagent(ctx, &subagent)
    })
    .result();

    assert!(!result.backgrounded);
    let (error, failed) = done_text(&result);
    assert!(failed, "{error}");
    assert!(error.contains(TIMED_OUT), "{error}");
    assert!(subagent.unsettled().is_empty());
}

/// A redaction layer must not leak a result just because the call moved: the
/// receipt can carry the call's output, and the final reply takes the output
/// layers before the chat, the task tab, or a waiting `task_stop` sees it.
#[test]
fn a_moved_calls_receipt_and_final_reply_pass_the_output_layers() {
    let host = Host::new();
    host.host
        .load_source(REDACTOR_PLUGIN, REDACTOR_SRC)
        .unwrap();
    let chat = Chat::live();
    let (receipt, _) = done_text(&moved(&host, &chat).result());
    assert_eq!(receipt, REDACTED_RECEIPT);

    host.release(MOVABLE);

    assert_eq!(done_text(&host.task_done()), (REDACTED.to_owned(), false));
    let notices = chat.mailbox.drain();
    let notice = notices
        .first()
        .and_then(|notice| notice.first_text_content())
        .expect("the notice reached the chat");
    assert!(notice.ends_with(REDACTED), "{notice}");
    assert!(!notice.contains(FINAL_REPLY), "{notice}");
}

/// The model reads a failed call, so nothing may report back on it later:
/// the move is abandoned before the receipt leaves dispatch.
#[test]
fn a_receipt_an_output_layer_fails_abandons_the_move() {
    let host = Host::new();
    host.host
        .load_source(RECEIPT_REFUSER_PLUGIN, RECEIPT_REFUSER_SRC)
        .unwrap();
    let chat = Chat::live();
    let result = moved(&host, &chat).result();

    assert!(!result.backgrounded);
    assert_eq!(done_text(&result), (RECEIPT_REFUSED.to_owned(), true));
    assert!(chat.mailbox.unsettled().is_empty());
}

/// The run ended while the call was moving, so the entry is refused. The
/// move gave up before landing: the call never reads as moved, and the
/// trigger it claimed ends it in the foreground.
#[test]
fn a_refused_entry_leaves_the_call_unmoved() {
    let host = Host::new();
    let chat = Chat::live();
    let mut scope = None;
    let running = Running::start_as(&host, chat.id, CALL_ID, MOVABLE, movable_input(), |ctx| {
        scope = ctx.call.clone();
    });
    running.wait_movable();
    chat.mailbox.close();

    assert_eq!(
        host.request(),
        MoveOutcome::NotMovable(RUN_ENDED.to_owned())
    );

    let scope = scope.expect("a model call has a scope");
    assert!(scope.home().is_none());
    assert!(scope.cancel().is_cancelled());
    host.release(MOVABLE);
    assert!(!running.result().backgrounded);
    assert!(chat.mailbox.unsettled().is_empty());
}

/// Esc fires while the hook is parked, so the commit finds the call already
/// ended: the move gives up, the call ends quietly and nothing reports back.
#[test]
fn an_esc_while_the_hook_is_parked_ends_the_call_quietly() {
    let host = Host::new();
    let chat = Chat::live();
    let input = json!({ "receipt": RECEIPT, "final": FINAL, "park_hook": true });
    let running = Running::start(&host, chat.id, MOVABLE, input);
    running.wait_movable();
    let outcome = host
        .host
        .event_handle()
        .request_background(CALL_ID.to_owned());
    running.wait_live_buf();

    running.run.cancel();
    host.release(HOOK);

    assert_eq!(
        outcome.recv_timeout(WAIT).expect("move outcome"),
        MoveOutcome::NotMovable(CANCELLED.to_owned())
    );
    host.release(MOVABLE);
    assert!(!running.call.join().expect("call thread").backgrounded);
    assert!(chat.mailbox.drain().is_empty());
    assert!(chat.mailbox.unsettled().is_empty());
}

/// The cap counts moved calls, and a call it refuses stays in the foreground.
#[test]
fn a_call_over_the_task_cap_stays_in_the_foreground() {
    let host = Host::new();
    let chat = Chat::live();
    for n in 0..TASK_CAP {
        let id = format!("{CALL_ID}_{n}");
        let running = Running::start_as(&host, chat.id, &id, MOVABLE, movable_input(), |_| {});
        running.wait_movable();
        assert_eq!(
            request_move(&host.host.event_handle(), &id),
            MoveOutcome::Moved
        );
        assert!(running.result().backgrounded);
    }

    let running = Running::start(&host, chat.id, MOVABLE, movable_input());
    running.wait_movable();
    assert_eq!(
        host.request(),
        MoveOutcome::NotMovable(format!("{TASK_CAP} {CAP_REACHED}"))
    );

    host.release(MOVABLE);
    assert!(!running.result().backgrounded);
}
