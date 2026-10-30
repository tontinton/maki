//! The bundled `task_stop` tool against calls moved to the background. A
//! parker tool registers a move hook and waits, standing in for a long
//! command.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use common::{
    Chat as LiveChat, WAIT, call_ctx, done_result, exec, next_task_done, request_move, wait_movable,
};
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentEvent, AgentSeed, CancelToken, CancelTrigger, EventSender, SessionMailbox};
use maki_config::{PluginsConfig, ToolOutputLines};
use maki_lua::{MoveOutcome, PluginHost, RestoreItem, RestoreReason, UiAction};
use maki_storage::id::MakiId;
use serde_json::{Value, json};
use test_case::test_case;

const STOP_TOOL: &str = "task_stop";
const PARK_TOOL: &str = "park";
const PARKER_PLUGIN: &str = "task_stop_parker";
const REDACTOR_PLUGIN: &str = "redactor";
const REDACTED: &str = "[redacted]";
const RECEIPT: &str = "receipt: moved to the background";
const PARTIAL_OUTPUT: &str = "partial output so far";
const STOPPED_REASON: &str = "stopped";
const CANCELLED_REASON: &str = "cancelled";
const STOP_REQUESTED_FRAGMENT: &str = "Stop requested";
const NO_RUNNING_TASK_FRAGMENT: &str = "no running background task";
const FIRST_ID: &str = "toolu_first";
const SECOND_ID: &str = "toolu_second";
const MISSING_ID: &str = "toolu_missing";
const HEADER_ID: &str = "toolu_header";
const HEADER_NAME: &str = "Wait then finish";
const SUBAGENT_TASK_ID: &str = "toolu_subagent";
const RUN_ID: u64 = 1;

/// With `honor_cancel` the handler returns at once and, like bash with its
/// job, finishes with the partial output and the cancel reason. Without it
/// the handler stays parked and ignores the stop, so only the runtime's
/// abandon timeout ends it.
const PARKER_SRC: &str = r#"
maki.api.register_tool({
  name = "park",
  description = "waits until cancelled",
  schema = {
    type = "object",
    properties = { honor_cancel = { type = "boolean" } },
  },
  audiences = { "main" },
  handler = function(input, ctx)
    ctx:on_background(function()
      return "receipt: moved to the background"
    end)
    if not input.honor_cancel then
      maki.async.await(1, function() end)
    end
    maki.async.on_cancel(function(reason)
      ctx:finish({ llm_output = "partial output so far: " .. reason })
    end)
    maki.fn.jobstart("sleep 600", {})
    return nil
  end,
})
"#;

/// An output layer on `park` that hides whatever the call ended with. It
/// leaves the receipt alone, so the move still reads as one.
const REDACTOR_SRC: &str = r#"
maki.api.set_slot("tool.park.output", function(prev, out, ctx)
  if not ctx.receipt then
    out.text = "[redacted]"
  end
  return prev(out, ctx)
end)
"#;

struct Chat {
    id: MakiId,
    mailbox: SessionMailbox,
    seed: Arc<AgentSeed>,
    _run_trigger: CancelTrigger,
    run: CancelToken,
}

fn chat() -> Chat {
    let LiveChat { id, mailbox, seed } = LiveChat::live();
    let (run_trigger, run) = CancelToken::new();
    Chat {
        id,
        mailbox,
        seed,
        _run_trigger: run_trigger,
        run,
    }
}

fn host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
        .unwrap();
    host.load_source(PARKER_PLUGIN, PARKER_SRC).unwrap();
    (reg, host)
}

/// A ctx of the chat's own agent, whose run reads the chat's mailbox.
fn agent_ctx(chat: &Chat) -> ToolContext {
    let mut ctx = chat.seed.tool_context(CancelToken::none());
    ctx.mailbox = Some(chat.mailbox.clone());
    ctx
}

fn stop(reg: &Arc<ToolRegistry>, chat: &Chat, id: &str) -> Result<String, String> {
    stop_as(reg, &agent_ctx(chat), id)
}

fn stop_as(reg: &Arc<ToolRegistry>, ctx: &ToolContext, id: &str) -> Result<String, String> {
    done_result(&exec(reg, STOP_TOOL, json!({ "id": id }), ctx))
}

/// Runs a `park` call as the chat's model call `id` and moves it. Returns once
/// the receipt answered the call.
fn park_and_move(
    reg: &Arc<ToolRegistry>,
    host: &PluginHost,
    chat: &Chat,
    id: &str,
    honor_cancel: bool,
) {
    let (ctx, events) = call_ctx(reg, chat.id, &chat.run, id);
    let (receipt_tx, receipt_rx) = flume::bounded(1);
    let reg = Arc::clone(reg);
    thread::spawn(move || {
        let input = json!({ "honor_cancel": honor_cancel });
        receipt_tx.send(exec(&reg, PARK_TOOL, input, &ctx)).unwrap();
    });

    wait_movable(&events, id);
    assert_eq!(request_move(&host.event_handle(), id), MoveOutcome::Moved);

    let receipt = receipt_rx.recv_timeout(WAIT).unwrap();
    assert!(receipt.backgrounded);
    assert!(done_result(&receipt).unwrap().contains(RECEIPT));
}

/// The chat's notice for a task that ended on its own. The host sends it
/// before the task tab's final reply, so that reply is the signal to look.
fn next_notice(chat: &Chat, ui: &flume::Receiver<UiAction>) -> (String, bool) {
    next_task_done(ui);
    let mut woken = chat.mailbox.claim_wake();
    let woke = !woken.is_empty();
    if !woke {
        woken = chat.mailbox.drain();
    }
    let notice = woken.first().expect("the notice precedes the final reply");
    (notice.first_text_content().unwrap().to_owned(), woke)
}

/// The cancel hook learns that the model stopped the call, so its marker can
/// say so.
#[test]
fn stopping_a_moved_call_returns_its_output_and_sends_no_notice() {
    let (reg, host) = host();
    let chat = chat();
    park_and_move(&reg, &host, &chat, FIRST_ID, true);

    let output = stop(&reg, &chat, FIRST_ID).unwrap();

    assert!(
        output.contains(&format!("{PARTIAL_OUTPUT}: {STOPPED_REASON}")),
        "{output}"
    );
    assert!(chat.mailbox.drain().is_empty());
    assert!(chat.mailbox.unsettled().is_empty());
}

/// The stop cancels the call before its final reply exists, and the layer
/// still has its say on that reply before the model reads it.
#[test]
fn a_stopped_calls_output_passes_its_output_layers() {
    let (reg, host) = host();
    host.load_source(REDACTOR_PLUGIN, REDACTOR_SRC).unwrap();
    let chat = chat();
    park_and_move(&reg, &host, &chat, FIRST_ID, true);

    let output = stop(&reg, &chat, FIRST_ID).unwrap();

    assert_eq!(output, REDACTED);
}

#[test]
fn a_user_stop_reaches_the_cancel_hook_as_a_cancel() {
    let (reg, host) = host();
    host.ui_attachment().attach();
    let ui = host.ui_action_rx();
    let chat = chat();
    park_and_move(&reg, &host, &chat, FIRST_ID, true);

    host.event_handle().cancel_plugin_session(FIRST_ID);

    let (notice, _) = next_notice(&chat, &ui);
    assert!(
        notice.contains(&format!("{PARTIAL_OUTPUT}: {CANCELLED_REASON}")),
        "{notice}"
    );
}

#[test]
fn a_stop_that_times_out_still_delivers_the_late_result_quietly() {
    let (reg, host) = host();
    host.ui_attachment().attach();
    let ui = host.ui_action_rx();
    let chat = chat();
    park_and_move(&reg, &host, &chat, FIRST_ID, false);

    let output = stop(&reg, &chat, FIRST_ID).unwrap();
    assert!(output.contains(STOP_REQUESTED_FRAGMENT), "{output}");

    let (notice, woke) = next_notice(&chat, &ui);
    assert!(notice.contains(PARK_TOOL), "{notice}");
    assert!(notice.contains(FIRST_ID), "{notice}");
    assert!(!woke, "a stopped task must not wake the chat");
}

#[test]
fn an_unknown_id_is_an_error_listing_the_running_ids() {
    let (reg, host) = host();
    let chat = chat();
    park_and_move(&reg, &host, &chat, FIRST_ID, true);
    park_and_move(&reg, &host, &chat, SECOND_ID, true);

    let err = stop(&reg, &chat, MISSING_ID).unwrap_err();

    assert!(err.contains(NO_RUNNING_TASK_FRAGMENT), "{err}");
    assert!(err.contains(FIRST_ID), "{err}");
    assert!(err.contains(SECOND_ID), "{err}");
    assert!(
        stop(&reg, &chat, FIRST_ID)
            .unwrap()
            .contains(PARTIAL_OUTPUT)
    );
    assert!(
        stop(&reg, &chat, SECOND_ID)
            .unwrap()
            .contains(PARTIAL_OUTPUT)
    );
}

#[test]
fn another_chats_id_is_refused_and_its_task_keeps_running() {
    let (reg, host) = host();
    let owner = chat();
    let other = chat();
    park_and_move(&reg, &host, &owner, FIRST_ID, true);

    let err = stop(&reg, &other, FIRST_ID).unwrap_err();

    assert!(err.contains(NO_RUNNING_TASK_FRAGMENT), "{err}");
    assert!(other.mailbox.drain().is_empty());
    assert!(
        stop(&reg, &owner, FIRST_ID)
            .unwrap()
            .contains(PARTIAL_OUTPUT)
    );
}

/// The chat's model moved the call, so a subagent of the chat, with a run
/// of its own, has nothing to stop.
#[test]
fn a_subagent_cannot_stop_a_task_its_chat_moved() {
    let (reg, host) = host();
    let chat = chat();
    park_and_move(&reg, &host, &chat, FIRST_ID, true);
    let mut subagent_ctx = chat.seed.tool_context(CancelToken::none());
    subagent_ctx.task_id = Some(Arc::from(SUBAGENT_TASK_ID));
    subagent_ctx.mailbox = Some(SessionMailbox::register(MakiId::generate()));

    let err = stop_as(&reg, &subagent_ctx, FIRST_ID).unwrap_err();

    assert!(err.contains(NO_RUNNING_TASK_FRAGMENT), "{err}");
    assert!(
        stop(&reg, &chat, FIRST_ID)
            .unwrap()
            .contains(PARTIAL_OUTPUT)
    );
}

/// The stop names the task the way the user knows it, and the id is what is
/// left when the name was never known.
#[test_case(None, HEADER_ID ; "without_a_name")]
#[test_case(Some(json!({ "name": HEADER_NAME })), HEADER_NAME ; "with_a_name")]
fn restore_shows_the_task_as_the_header(state: Option<Value>, expected: &str) {
    let (_reg, host) = host();
    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded();
    let item = RestoreItem {
        tool: Arc::from(STOP_TOOL),
        tool_use_id: "restore_id".to_owned(),
        output: PARTIAL_OUTPUT.to_owned(),
        input: json!({ "id": HEADER_ID }),
        is_error: false,
        tool_output_lines: ToolOutputLines::DEFAULT,
        theme_gen: None,
        clicks: Vec::new(),
        state,
        session_id: None,
        task_id: None,
        reason: RestoreReason::default(),
    };

    handle.request_restore(item, EventSender::new(tx, RUN_ID));
    handle.wait_restore_complete_for_test();

    let header = rx
        .drain()
        .find_map(|env| match env.event {
            AgentEvent::ToolHeaderSnapshot { snapshot, .. } => Some(snapshot.text()),
            _ => None,
        })
        .expect("restore emits a header");
    assert_eq!(header, expected);
}
