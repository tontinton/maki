//! The bundled `task_stop` tool against calls moved to the background. A
//! parker tool registers a move hook and waits, standing in for a long
//! command.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use maki_agent::tools::test_support::stub_seed;
use maki_agent::tools::{ToolAudience, ToolContext, ToolExecResult, ToolRegistry};
use maki_agent::{
    AgentEvent, AgentSeed, CallScope, CancelToken, CancelTrigger, EventSender, SessionMailbox,
    ToolOutput,
};
use maki_config::PluginsConfig;
use maki_lua::{MoveOutcome, PluginHost, RestoreItem, RestoreReason, UiAction};
use maki_storage::id::MakiId;
use serde_json::{Value, json};

const STOP_TOOL: &str = "task_stop";
const PARK_TOOL: &str = "park";
const PARKER_PLUGIN: &str = "task_stop_parker";
const RECEIPT: &str = "receipt: moved to the background";
const PARTIAL_OUTPUT: &str = "partial output so far";
const STOP_REQUESTED_FRAGMENT: &str = "Stop requested";
const NO_RUNNING_TASK_FRAGMENT: &str = "no running background task";
const FIRST_ID: &str = "toolu_first";
const SECOND_ID: &str = "toolu_second";
const MISSING_ID: &str = "toolu_missing";
const HEADER_ID: &str = "toolu_header";
const RUN_ID: u64 = 1;
const DEADLINE: Duration = Duration::from_secs(60);

/// `honor_cancel` returns at once and finishes with the partial output when
/// the call is cancelled, as bash does with its job. Without it the handler
/// stays parked and ignores the stop, so only the runtime's abandon timeout
/// ends it.
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
    maki.async.on_cancel(function()
      ctx:finish({ llm_output = "partial output so far" })
    end)
    maki.fn.jobstart("sleep 600", {})
    return nil
  end,
})
"#;

struct Chat {
    mailbox: SessionMailbox,
    seed: Arc<AgentSeed>,
    _run_trigger: CancelTrigger,
    run: CancelToken,
}

fn chat() -> Chat {
    let id = MakiId::generate();
    let mailbox = SessionMailbox::register(id);
    let seed = mailbox.publish_seed(stub_seed(id));
    let (run_trigger, run) = CancelToken::new();
    Chat {
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

fn exec(reg: &ToolRegistry, name: &str, input: Value, ctx: &ToolContext) -> ToolExecResult {
    let inv = reg.get(name).unwrap().tool.parse(&input).unwrap();
    smol::block_on(inv.execute(ctx))
}

fn text(result: ToolExecResult) -> Result<String, String> {
    match result.output? {
        ToolOutput::Plain(s) | ToolOutput::Markdown(s) => Ok(s.text),
        other => panic!("unexpected output {other:?}"),
    }
}

fn stop(reg: &ToolRegistry, chat: &Chat, id: &str) -> Result<String, String> {
    let ctx = chat.seed.tool_context(CancelToken::none());
    text(exec(reg, STOP_TOOL, json!({ "id": id }), &ctx))
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
    let (events_tx, events_rx) = flume::unbounded();
    let scope = Arc::new(CallScope::new(id.to_owned(), &chat.run));
    let mut ctx = chat.seed.tool_context(CancelToken::none());
    ctx.event_tx = EventSender::new(events_tx, RUN_ID);
    ctx.tool_use_id = Some(id.to_owned());
    ctx.cancel = scope.cancel().clone();
    ctx.call = Some(scope);

    let (receipt_tx, receipt_rx) = flume::bounded(1);
    let reg = Arc::clone(reg);
    thread::spawn(move || {
        let input = json!({ "honor_cancel": honor_cancel });
        receipt_tx.send(exec(&reg, PARK_TOOL, input, &ctx)).unwrap();
    });

    let deadline = Instant::now() + DEADLINE;
    loop {
        let envelope = events_rx
            .recv_deadline(deadline)
            .expect("the call never announced it can move");
        if matches!(
            envelope.event,
            AgentEvent::ToolMovable { movable: true, .. }
        ) {
            break;
        }
    }
    let outcome = host
        .event_handle()
        .request_background(id.to_owned())
        .recv_timeout(DEADLINE)
        .unwrap();
    assert_eq!(outcome, MoveOutcome::Moved);

    let receipt = receipt_rx.recv_timeout(DEADLINE).unwrap();
    assert!(receipt.backgrounded);
    assert!(text(receipt).unwrap().contains(RECEIPT));
}

/// The chat's notice for a task that ended on its own. The host sends it
/// before the task tab's final reply, so that reply is the signal to look.
fn next_notice(chat: &Chat, ui: &flume::Receiver<UiAction>) -> (String, bool) {
    loop {
        let action = ui.recv_timeout(DEADLINE).expect("no final reply arrived");
        if let UiAction::PluginTaskEvent { envelope, .. } = action
            && matches!(envelope.event, AgentEvent::ToolDone(_))
        {
            break;
        }
    }
    let mut woken = chat.mailbox.claim_wake();
    let woke = !woken.is_empty();
    if !woke {
        woken = chat.mailbox.drain();
    }
    let notice = woken.first().expect("the notice precedes the final reply");
    (notice.first_text_content().unwrap().to_owned(), woke)
}

#[test]
fn task_stop_is_registered_for_the_main_audience_only() {
    let (reg, _host) = host();
    let entry = reg.get(STOP_TOOL).expect("task_stop is always registered");
    assert_eq!(entry.tool.audience(), ToolAudience::MAIN);
}

#[test]
fn stopping_a_moved_call_returns_its_output_and_sends_no_notice() {
    let (reg, host) = host();
    let chat = chat();
    park_and_move(&reg, &host, &chat, FIRST_ID, true);

    let output = stop(&reg, &chat, FIRST_ID).unwrap();

    assert!(output.contains(PARTIAL_OUTPUT), "{output}");
    assert!(chat.mailbox.drain().is_empty());
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

#[test]
fn restore_shows_the_id_as_the_header() {
    let (_reg, host) = host();
    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded();
    let item = RestoreItem {
        tool: Arc::from(STOP_TOOL),
        tool_use_id: "restore_id".to_owned(),
        output: PARTIAL_OUTPUT.to_owned(),
        input: json!({ "id": HEADER_ID }),
        is_error: false,
        tool_output_lines: maki_config::ToolOutputLines::DEFAULT,
        theme_gen: None,
        clicks: Vec::new(),
        state: None,
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
    assert_eq!(header, HEADER_ID);
}
