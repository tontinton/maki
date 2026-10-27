//! Calls that move themselves with `ctx:background()`: the real task and bash
//! plugins asked to run in the background, and the `agent` gate on the move.
//! Model selection inherits the test provider; the session APIs stay real.

#[cfg(unix)]
use std::collections::HashMap;
#[cfg(unix)]
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use maki_agent::permissions::PermissionManager;
use maki_agent::tools::test_support::{stub_ctx_with, stub_seed};
use maki_agent::tools::{ToolContext, ToolExecResult, ToolRegistry};
use maki_agent::{
    AgentEvent, AgentMode, AgentSeed, CallScope, CancelToken, Envelope, EventSender,
    PermissionsConfig, SessionMailbox, ToolDoneEvent, ToolOutput,
};
use maki_config::ProjectConfig;
#[cfg(unix)]
use maki_config::{PluginFileConfig, PluginsConfig};
use maki_lua::{Permission, PluginHost, PluginPermissions, UiAction};
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::{
    AgentError, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, TokenUsage,
};
use maki_storage::id::{MakiId, SessionRef};
#[cfg(unix)]
use maki_storage::paths::{session_task_logs_dir, state_dir};
use serde_json::{Value, json};
use tempfile::TempDir;
use test_case::test_case;

const PLUGIN: &str = "background_task_test";
const GUARDED_PLUGIN: &str = "guarded_tool_test";
const MOVER_PLUGIN: &str = "self_mover_test";
const TASK_TOOL: &str = "task";
const MOVER_TOOL: &str = "self_mover";
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
const WAIT: Duration = Duration::from_secs(10);
const DEFAULT_SLOTS: u64 = 8;
const INIT: &str = include_str!("../../plugins/task/init.lua");
const RUNNER: &str = include_str!("../../plugins/task/runner.lua");

// Mirrors of the runtime's wording.
const MOVED_AS_ASKED: &str = "Started in the background as asked";
const FOREGROUND_NOTE_HEAD: &str = "Could not start in the background (";
const FOREGROUND_NOTE_TAIL: &str = "Ran in the foreground.";
const AGENT_DENIED: &str = "permission denied: 'agent' not granted";

#[cfg(unix)]
const STATE_ENV_VARS: [&str; 2] = ["HOME", "XDG_STATE_HOME"];
#[cfg(unix)]
const BASH_TOOL: &str = "bash";
#[cfg(unix)]
const BASH_RECEIPT: &str = "Running in the background.";
#[cfg(unix)]
const BASH_PROGRESS_LABEL: &str = "Full output (read it to check progress): ";
#[cfg(unix)]
const BASH_FINAL_LINE: &str = "finished-late";
#[cfg(unix)]
const GATE_FILE: &str = "go";
#[cfg(unix)]
const GATE_POLL_SECS: &str = "0.02";

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
"#;

/// Answers once released. The first turn calls `guarded` when asked to.
struct ParkedProvider {
    started: flume::Sender<()>,
    release: flume::Receiver<()>,
    calls_guarded: AtomicBool,
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
            let (block, stop_reason) = if self.calls_guarded.swap(false, Ordering::SeqCst) {
                (
                    ContentBlock::tool_use(GUARDED_CALL_ID, GUARDED_TOOL, json!({ "target": "x" })),
                    StopReason::ToolUse,
                )
            } else {
                (
                    ContentBlock::Text {
                        text: RESULT.into(),
                    },
                    StopReason::EndTurn,
                )
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

struct Parked {
    provider: Arc<dyn Provider>,
    started: flume::Receiver<()>,
    release: flume::Sender<()>,
}

impl Parked {
    fn new(calls_guarded: bool) -> Self {
        let (started_tx, started) = flume::unbounded();
        let (release, release_rx) = flume::unbounded();
        Self {
            provider: Arc::new(ParkedProvider {
                started: started_tx,
                release: release_rx,
                calls_guarded: AtomicBool::new(calls_guarded),
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

    fn task_event(&self) -> Envelope {
        loop {
            if let UiAction::PluginTaskEvent { envelope, .. } =
                self.ui.recv_timeout(WAIT).expect("task tab event")
            {
                return *envelope;
            }
        }
    }

    /// The final reply of the moved call, as its task tab gets it.
    fn task_done(&self) -> ToolDoneEvent {
        self.task_dones(&[CALL_ID]).remove(0)
    }

    /// The final replies of {ids}, in that order, whichever ends first.
    fn task_dones(&self, ids: &[&str]) -> Vec<ToolDoneEvent> {
        let mut found: Vec<Option<ToolDoneEvent>> = ids.iter().map(|_| None).collect();
        while found.iter().any(Option::is_none) {
            let envelope = self.task_event();
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
            let envelope = self.task_event();
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

/// A model call as dispatch makes it: one scope under the run token, in the
/// chat {chat}. The receiver keeps the call's live events flowing.
fn call_ctx(
    reg: &Arc<ToolRegistry>,
    chat: MakiId,
    run: &CancelToken,
) -> (ToolContext, flume::Receiver<Envelope>) {
    call_ctx_as(reg, chat, run, CALL_ID)
}

fn call_ctx_as(
    reg: &Arc<ToolRegistry>,
    chat: MakiId,
    run: &CancelToken,
    call_id: &str,
) -> (ToolContext, flume::Receiver<Envelope>) {
    let (tx, events) = flume::unbounded();
    let mut ctx = stub_ctx_with(
        &AgentMode::Build,
        Some(&EventSender::new(tx, 0)),
        Some(call_id),
    );
    let scope = Arc::new(CallScope::new(call_id.to_owned(), run));
    ctx.cancel = scope.cancel().clone();
    ctx.call = Some(scope);
    ctx.session_id = Some(SessionRef::from(chat));
    ctx.registry = Arc::clone(reg);
    (ctx, events)
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

/// The provider stays parked until released, so the receipt coming back at
/// all means the call did not wait for its subagent.
#[test]
fn a_background_task_returns_a_receipt_and_its_reply_arrives_as_the_notice() {
    let host = Host::task(Arc::new(ToolRegistry::new()));
    let chat = Chat::live();
    let parked = Parked::new(false);
    let (_run, run_token) = CancelToken::new();
    let (mut ctx, _events) = call_ctx(&host.reg, chat.id, &run_token);
    ctx.provider = Arc::clone(&parked.provider);

    let result = exec(&host.reg, TASK_TOOL, task_input(), &ctx);

    assert!(result.backgrounded);
    let (receipt, failed) = text(result);
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

/// Headless, SDK and ACP runs have no live chat to deliver to.
#[test]
fn a_background_task_without_a_live_chat_runs_blocking() {
    let host = Host::task(Arc::new(ToolRegistry::new()));
    let parked = Parked::new(false);
    let (_run, run_token) = CancelToken::new();
    let (mut ctx, _events) = call_ctx(&host.reg, MakiId::generate(), &run_token);
    ctx.provider = Arc::clone(&parked.provider);
    let reg = Arc::clone(&host.reg);
    let call = thread::spawn(move || exec(&reg, TASK_TOOL, task_input(), &ctx));

    parked.wait_started();
    assert!(!call.is_finished());
    parked.release.send(()).unwrap();

    let result = call.join().expect("call thread");
    assert!(!result.backgrounded);
    let (reply, failed) = text(result);
    assert!(!failed, "{reply}");
    assert!(reply.starts_with(FOREGROUND_NOTE_HEAD), "{reply}");
    assert!(reply.contains(FOREGROUND_NOTE_TAIL), "{reply}");
    assert!(reply.ends_with(RESULT), "{reply}");
}

/// The move comes before the wait for a slot, so a full house does not hold
/// the model's turn. The second task keeps waiting in the background.
#[test]
fn a_background_task_waiting_for_a_slot_returns_its_receipt_at_once() {
    let host = Host::task_with_slots(Arc::new(ToolRegistry::new()), 1);
    let chat = Chat::live();
    let parked = Parked::new(false);
    let (_run, run_token) = CancelToken::new();
    let (mut first, _first_events) = call_ctx(&host.reg, chat.id, &run_token);
    first.provider = Arc::clone(&parked.provider);
    let (mut second, _second_events) = call_ctx_as(&host.reg, chat.id, &run_token, SECOND_CALL_ID);
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
    let (receipt, failed) = text(result);
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
    let parked = Parked::new(true);
    parked.release.send(()).unwrap();
    let project = TempDir::new().unwrap();
    let (_run, run_token) = CancelToken::new();
    let (mut ctx, _events) = call_ctx(&reg, chat.id, &run_token);
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
    let (ctx, _events) = call_ctx(&host.reg, chat.id, &run_token);

    let result = exec(&host.reg, MOVER_TOOL, json!({}), &ctx);

    assert_eq!(result.backgrounded, agent);
    let (reply, failed) = text(result);
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

/// Lets the gated command finish whatever happens to the test.
#[cfg(unix)]
struct Gate(TempDir);

#[cfg(unix)]
impl Gate {
    fn release(&self) {
        fs::write(self.0.path().join(GATE_FILE), "").unwrap();
    }
}

#[cfg(unix)]
impl Drop for Gate {
    fn drop(&mut self) {
        fs::write(self.0.path().join(GATE_FILE), "").ok();
    }
}

/// Points the state directory at a throwaway tree, so the task log never
/// lands in the user's. Dropping the tree is the cleanup.
#[cfg(unix)]
fn isolated_state() -> TempDir {
    let dir = TempDir::new().unwrap();
    for var in STATE_ENV_VARS {
        // SAFETY: setting a variable is only sound while no other thread reads
        // the environment. Every test runs in its own process under nextest
        // and calls this before it starts a thread.
        unsafe { std::env::set_var(var, dir.path()) };
    }
    dir
}

#[cfg(unix)]
#[test]
fn bash_in_the_background_returns_a_receipt_with_the_log_path_at_once() {
    let _state = isolated_state();
    let reg = Arc::new(ToolRegistry::new());
    let mut plugins = PluginHost::new(Arc::clone(&reg)).unwrap();
    let config = PluginsConfig::from_plugins(HashMap::from([(
        BASH_TOOL.to_owned(),
        PluginFileConfig::default(),
    )]));
    plugins.load_builtins(&config).unwrap();
    let host = Host::attached(reg, plugins);
    let chat = Chat::live();
    let gate = Gate(TempDir::new().unwrap());
    let command = format!(
        "echo started; while [ ! -e '{}' ]; do sleep {GATE_POLL_SECS}; done; echo {BASH_FINAL_LINE}",
        gate.0.path().join(GATE_FILE).display()
    );
    let (_run, run_token) = CancelToken::new();
    let (mut ctx, _events) = call_ctx(&host.reg, chat.id, &run_token);
    ctx.config.rtk = false;
    let logs = session_task_logs_dir(&state_dir().unwrap(), chat.id);
    let log = logs.join(format!("{CALL_ID}.log"));

    let result = exec(
        &host.reg,
        BASH_TOOL,
        json!({ "command": command, "background": true }),
        &ctx,
    );

    assert!(result.backgrounded);
    let (receipt, failed) = text(result);
    assert!(!failed, "{receipt}");
    assert!(receipt.starts_with(BASH_RECEIPT), "{receipt}");
    assert!(
        receipt.contains(&format!("{BASH_PROGRESS_LABEL}{}", log.display())),
        "{receipt}"
    );
    assert!(receipt.contains(MOVED_AS_ASKED), "{receipt}");

    gate.release();
    let (output, failed) = done_text(&host.task_done());
    assert!(!failed, "{output}");
    assert!(output.contains(BASH_FINAL_LINE), "{output}");
}
