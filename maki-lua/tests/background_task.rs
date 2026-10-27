//! Calls that move themselves with `ctx:background()`: the real task plugin
//! asked to run in the background, and the `agent` gate on the move.
//! Model selection inherits the test provider; the session APIs stay real.

mod common;

use std::sync::Arc;
use std::thread;

use common::{Chat, Parked, WAIT, call_ctx, done_text, exec, next_task_event};
use maki_agent::permissions::PermissionManager;
use maki_agent::tools::ToolRegistry;
use maki_agent::{AgentEvent, CancelToken, PermissionsConfig, SessionMailbox, ToolDoneEvent};
use maki_config::ProjectConfig;
use maki_lua::{Permission, PluginHost, PluginPermissions, UiAction};
use maki_providers::ContentBlock;
use maki_storage::id::MakiId;
use serde_json::{Value, json};
use tempfile::TempDir;
use test_case::test_case;

const PLUGIN: &str = "background_task_test";
const GUARDED_PLUGIN: &str = "guarded_tool_test";
const MOVER_PLUGIN: &str = "self_mover_test";
const TASK_TOOL: &str = "task";
const MOVER_TOOL: &str = "self_mover";
const DEADLINE_MOVER_TOOL: &str = "deadline_mover";
const CALL_ID: &str = "toolu_background_task";
const SECOND_CALL_ID: &str = "toolu_background_task_second";
const GUARDED_CALL_ID: &str = "toolu_guarded";
const GUARDED_TOOL: &str = "guarded";
const NAME: &str = "find the answer";
const PROMPT: &str = "Find the answer";
const RESULT: &str = "background answer";
const MOVED_RECEIPT: &str = "Subagent find the answer moved to the background";
const MOVER_RECEIPT: &str = "mover moved";
const MOVER_FINAL: &str = "true: nil";
const DEFAULT_SLOTS: u64 = 8;
const INIT: &str = include_str!("../../plugins/task/init.lua");
const RUNNER: &str = include_str!("../../plugins/task/runner.lua");

// Mirrors of the runtime's wording.
const MOVED_AS_ASKED: &str = "Started in the background as asked";
const AGENT_DENIED: &str = "permission denied: 'agent' not granted";
const TIMED_OUT: &str = "timed out after";
const SUBAGENT_SELF_MOVE: &str =
    "background: a subagent's tool call only moves when the user moves it";
const SUBAGENT_TASK_ID: &str = "toolu_subagent";
const NESTED_CALL: &str = "background is ignored for a call inside batch or another tool; \
     call the tool directly to run it in the background";
const NO_LIVE_CHAT: &str = "no live chat";
const BACKGROUND_OFF: &str = "background tasks are turned off here (--background-timeout 0)";

/// A tool only a subagent sees, whose every call needs the user's answer.
const GUARDED_SRC: &str = r#"
maki.api.register_tool({
  name = "guarded",
  description = "needs a yes from the user",
  schema = { type = "object", properties = { target = { type = "string" } } },
  audiences = { "research_sub" },
  permission = "run",
  permission_scopes = "target",
  handler = function()
    return "ran"
  end,
})
"#;

const MOVER_SRC: &str = r#"
maki.api.register_tool({
  name = "self_mover",
  description = "moves itself to the background",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  handler = function(_, ctx)
    ctx:on_background(function()
      return "mover moved"
    end)
    local ok, err = ctx:background()
    return tostring(ok) .. ": " .. tostring(err)
  end,
})
maki.api.register_tool({
  name = "deadline_mover",
  description = "moves itself when its deadline passes",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  handler = function(_, ctx)
    ctx:on_background(function()
      return "mover moved"
    end)
    ctx:set_deadline(0, { expire = "background" })
    maki.async.await(1, function() end)
  end,
})
"#;

struct Host {
    reg: Arc<ToolRegistry>,
    host: PluginHost,
    ui: flume::Receiver<UiAction>,
}

impl Host {
    fn attached(reg: Arc<ToolRegistry>, host: PluginHost) -> Self {
        host.ui_attachment().attach();
        let ui = host.ui_action_rx();
        Self { reg, host, ui }
    }

    /// The real task plugin with `background` exposed.
    fn task(reg: Arc<ToolRegistry>) -> Self {
        Self::task_with_slots(reg, DEFAULT_SLOTS)
    }

    fn task_with_slots(reg: Arc<ToolRegistry>, slots: u64) -> Self {
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
        host.load_source_with_opts(
            PLUGIN,
            &task_source(),
            serde_json::Map::from_iter([
                ("allow_background".into(), json!(true)),
                ("max_concurrent".into(), json!(slots)),
            ]),
        )
        .unwrap();
        Self::attached(reg, host)
    }

    fn mover(agent: bool) -> Self {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
        let mut permissions = PluginPermissions::trusted();
        permissions.set(Permission::Agent, agent);
        host.load_source_with_permissions(MOVER_PLUGIN, MOVER_SRC, permissions)
            .unwrap();
        Self::attached(reg, host)
    }

    /// The final reply of the moved call, as its task tab gets it.
    fn task_done(&self) -> ToolDoneEvent {
        self.task_dones(&[CALL_ID]).remove(0)
    }

    /// The final replies of {ids}, in that order, whichever ends first.
    fn task_dones(&self, ids: &[&str]) -> Vec<ToolDoneEvent> {
        let mut found: Vec<Option<ToolDoneEvent>> = ids.iter().map(|_| None).collect();
        while found.iter().any(Option::is_none) {
            let envelope = next_task_event(&self.ui);
            if let (AgentEvent::ToolDone(done), None) = (envelope.event, envelope.subagent)
                && let Some(slot) = ids.iter().position(|id| *id == done.id)
            {
                found[slot] = Some(*done);
            }
        }
        found.into_iter().flatten().collect()
    }

    /// Waits for the moved subagent to ask the user, failing if it ends first.
    fn wait_permission_request(&self) {
        loop {
            let envelope = next_task_event(&self.ui);
            match (envelope.event, envelope.subagent) {
                (AgentEvent::PermissionRequest { .. }, _) => return,
                (AgentEvent::ToolDone(done), None) if done.id == CALL_ID => {
                    panic!("the task ended without asking: {}", done.output.as_text())
                }
                _ => {}
            }
        }
    }
}

fn task_source() -> String {
    format!(
        r#"
local builtin_require = require
local runner
local function require(name)
  if name == "runner" then
    if runner == nil then
      runner = (function()
{RUNNER}
      end)()
    end
    return runner
  end
  return builtin_require(name)
end
maki.agent.resolve_model = function() return {{}} end
{INIT}
"#
    )
}

fn task_input() -> Value {
    json!({ "description": NAME, "prompt": PROMPT, "background": true })
}

/// The provider stays parked until released, so the receipt coming back at
/// all means the call did not wait for its subagent.
#[test]
fn a_background_task_returns_a_receipt_and_its_reply_arrives_as_the_notice() {
    let host = Host::task(Arc::new(ToolRegistry::new()));
    let chat = Chat::live();
    let parked = Parked::new(RESULT);
    let (_run, run_token) = CancelToken::new();
    let (mut ctx, _events) = call_ctx(&host.reg, chat.id, &run_token, CALL_ID);
    ctx.provider = Arc::clone(&parked.provider);

    let result = exec(&host.reg, TASK_TOOL, task_input(), &ctx);

    assert!(result.backgrounded);
    let (receipt, failed) = done_text(&result);
    assert!(!failed, "{receipt}");
    assert!(receipt.starts_with(MOVED_RECEIPT), "{receipt}");
    assert!(receipt.contains(MOVED_AS_ASKED), "{receipt}");

    parked.wait_started();
    parked.release.send(()).unwrap();
    assert_eq!(done_text(&host.task_done()), (RESULT.to_owned(), false));
    let notices = chat.mailbox.drain();
    assert_eq!(notices.len(), 1);
    let notice = notices[0].first_text_content().unwrap();
    assert!(notice.contains(RESULT), "{notice}");
}

/// The move comes before the wait for a slot, so a full house does not hold
/// the model's turn. The second task keeps waiting in the background.
#[test]
fn a_background_task_waiting_for_a_slot_returns_its_receipt_at_once() {
    let host = Host::task_with_slots(Arc::new(ToolRegistry::new()), 1);
    let chat = Chat::live();
    let parked = Parked::new(RESULT);
    let (_run, run_token) = CancelToken::new();
    let (mut first, _first_events) = call_ctx(&host.reg, chat.id, &run_token, CALL_ID);
    first.provider = Arc::clone(&parked.provider);
    let (mut second, _second_events) = call_ctx(&host.reg, chat.id, &run_token, SECOND_CALL_ID);
    second.provider = Arc::clone(&parked.provider);

    assert!(exec(&host.reg, TASK_TOOL, task_input(), &first).backgrounded);
    parked.wait_started();

    let reg = Arc::clone(&host.reg);
    let (receipt_tx, receipt_rx) = flume::bounded(1);
    let waiter = thread::spawn(move || {
        receipt_tx
            .send(exec(&reg, TASK_TOOL, task_input(), &second))
            .ok();
    });
    let result = receipt_rx
        .recv_timeout(WAIT)
        .expect("the receipt must not wait for a free slot");
    waiter.join().expect("waiter thread");

    assert!(result.backgrounded);
    let (receipt, failed) = done_text(&result);
    assert!(!failed, "{receipt}");
    assert!(receipt.starts_with(MOVED_RECEIPT), "{receipt}");
    assert!(parked.started.try_recv().is_err(), "the slot is still held");

    parked.release.send(()).unwrap();
    parked.release.send(()).unwrap();
    for done in host.task_dones(&[CALL_ID, SECOND_CALL_ID]) {
        assert_eq!(done_text(&done), (RESULT.to_owned(), false));
    }
}

/// A ctx session has an answer channel, so the prompt reaches the user
/// instead of being denied as nobody's to answer. The subagent dispatches
/// through the global registry, so the tools live there.
#[test]
fn a_background_subagent_asks_for_permission_instead_of_being_denied() {
    let reg = Arc::clone(ToolRegistry::global_arc());
    let host = Host::task(Arc::clone(&reg));
    host.host.load_source(GUARDED_PLUGIN, GUARDED_SRC).unwrap();
    let chat = Chat::live();
    let parked = Parked::with_first_turn(
        RESULT,
        Some(ContentBlock::tool_use(
            GUARDED_CALL_ID,
            GUARDED_TOOL,
            json!({ "target": "x" }),
        )),
    );
    parked.release.send(()).unwrap();
    let project = TempDir::new().unwrap();
    let (_run, run_token) = CancelToken::new();
    let (mut ctx, _events) = call_ctx(&reg, chat.id, &run_token, CALL_ID);
    ctx.provider = Arc::clone(&parked.provider);
    ctx.permissions = Arc::new(PermissionManager::new(
        PermissionsConfig::default(),
        project.path().to_owned(),
        ProjectConfig::discover(project.path()),
        Arc::default(),
    ));
    let (_answer_tx, answer_rx) = flume::unbounded::<String>();
    ctx.user_response_rx = Some(Arc::new(smol::lock::Mutex::new(answer_rx)));

    assert!(exec(&reg, TASK_TOOL, task_input(), &ctx).backgrounded);

    host.wait_permission_request();
    host.host.event_handle().cancel_plugin_session(CALL_ID);
    host.task_done();
}

/// Moving itself makes the result wake the chat, the price of
/// `notify{ wake = true }`, so it takes the same `agent` grant.
#[test_case(true ; "with_agent_it_moves")]
#[test_case(false ; "without_agent_it_stays")]
fn ctx_background_needs_the_agent_permission(agent: bool) {
    let host = Host::mover(agent);
    let chat = Chat::live();
    let (_run, run_token) = CancelToken::new();
    let (ctx, _events) = call_ctx(&host.reg, chat.id, &run_token, CALL_ID);

    let result = exec(&host.reg, MOVER_TOOL, json!({}), &ctx);

    assert_eq!(result.backgrounded, agent);
    let (reply, failed) = done_text(&result);
    assert!(!failed, "{reply}");
    if agent {
        assert!(reply.starts_with(MOVER_RECEIPT), "{reply}");
        assert_eq!(
            done_text(&host.task_done()),
            (MOVER_FINAL.to_owned(), false)
        );
    } else {
        assert!(reply.contains(AGENT_DENIED), "{reply}");
        assert!(chat.mailbox.drain().is_empty());
    }
}

/// A deadline that expires to the background is a self-move all the same.
#[test_case(true ; "with_agent_it_moves")]
#[test_case(false ; "without_agent_it_times_out")]
fn a_background_deadline_needs_the_agent_permission(agent: bool) {
    let host = Host::mover(agent);
    let chat = Chat::live();
    let (_run, run_token) = CancelToken::new();
    let (ctx, _events) = call_ctx(&host.reg, chat.id, &run_token, CALL_ID);

    let result = exec(&host.reg, DEADLINE_MOVER_TOOL, json!({}), &ctx);

    assert_eq!(result.backgrounded, agent);
    let (reply, failed) = done_text(&result);
    if agent {
        assert!(reply.starts_with(MOVER_RECEIPT), "{reply}");
    } else {
        assert!(failed && reply.contains(TIMED_OUT), "{reply}");
        assert!(chat.mailbox.unsettled().is_empty());
    }
}

/// A subagent waits for its moved calls, so only the user moves one.
#[test]
fn ctx_background_refuses_a_subagents_call() {
    let host = Host::mover(true);
    let chat = Chat::live();
    let subagent = SessionMailbox::register(MakiId::generate());
    let (_run, run_token) = CancelToken::new();
    let (mut ctx, _events) = call_ctx(&host.reg, chat.id, &run_token, CALL_ID);
    ctx.task_id = Some(Arc::from(SUBAGENT_TASK_ID));
    ctx.mailbox = Some(subagent.clone());

    let result = exec(&host.reg, MOVER_TOOL, json!({}), &ctx);

    assert!(!result.backgrounded);
    assert_eq!(
        done_text(&result),
        (format!("nil: {SUBAGENT_SELF_MOVE}"), false)
    );
    assert!(subagent.unsettled().is_empty());
}

/// A call nested in batch shares the batch's scope but has no id of its own,
/// and the model has to learn to make it a direct call instead.
#[test]
fn ctx_background_refuses_a_call_nested_in_another_tool() {
    let host = Host::mover(true);
    let chat = Chat::live();
    let (_run, run_token) = CancelToken::new();
    let (mut ctx, _events) = call_ctx(&host.reg, chat.id, &run_token, CALL_ID);
    ctx.tool_use_id = None;

    let result = exec(&host.reg, MOVER_TOOL, json!({}), &ctx);

    assert!(!result.backgrounded);
    assert_eq!(done_text(&result), (format!("nil: {NESTED_CALL}"), false));
}

/// A session that is up without a seed runs with background work off, which
/// the model must not read as a missing chat.
#[test_case(false, NO_LIVE_CHAT ; "no_live_chat")]
#[test_case(true, BACKGROUND_OFF ; "background_off")]
fn ctx_background_without_a_seed_names_the_cause(session_up: bool, reason: &str) {
    let host = Host::mover(true);
    let chat = MakiId::generate();
    let _mailbox = session_up.then(|| SessionMailbox::register(chat));
    let (_run, run_token) = CancelToken::new();
    let (ctx, _events) = call_ctx(&host.reg, chat, &run_token, CALL_ID);

    let result = exec(&host.reg, MOVER_TOOL, json!({}), &ctx);

    assert!(!result.backgrounded);
    assert_eq!(done_text(&result), (format!("nil: {reason}"), false));
}
