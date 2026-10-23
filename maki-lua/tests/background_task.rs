//! Real task plugin and background sessions with a callback-controlled provider.
//! Model selection inherits the test provider; the session APIs stay real.

use std::sync::Arc;
use std::thread;

use maki_agent::tools::test_support::stub_seed;
use maki_agent::tools::{ToolContext, ToolExecResult, ToolRegistry};
use maki_agent::{AgentSeed, CancelToken, ModelSlot, SessionMailbox, ToolOutput};
use maki_lua::{PluginHost, SessionEndReason};
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::{
    AgentError, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, TokenUsage,
};
use maki_storage::id::{MakiId, SessionRef};
use serde_json::{Value, json};
use test_case::test_case;

const RESULT: &str = "background answer";
const LIST_TOOL: &str = "background_sessions";
const NOTICE_TOOL: &str = "next_notice";
const FIRST: &str = "first background task";
const SECOND: &str = "second background task";
const STOPPED_BY_USER_ERR: &str = "stopped by user";
const STARTED: &str = "Background task started";
const FINISHED: &str = "finished";
const FAILED: &str = "failed";
const PROMPT: &str = "Find the answer";
const PLUGIN: &str = "background_task_test";
const INIT: &str = include_str!("../../plugins/task/init.lua");
const RUNNER: &str = include_str!("../../plugins/task/runner.lua");
const BACKGROUND: &str = include_str!("../../plugins/task/background.lua");

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
            self.started.send(()).unwrap();
            self.release.recv_async().await.unwrap();
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

fn host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    load(&host);
    (reg, host)
}

fn load(host: &PluginHost) {
    let source = format!(
        r#"
local builtin_require = require
local modules, loaded = {{}}, {{}}
local require
require = function(name)
  if modules[name] then
    if not loaded[name] then loaded[name] = modules[name]() end
    return loaded[name]
  end
  return builtin_require(name)
end
modules.runner = function()
{RUNNER}
end
modules.background = function()
{BACKGROUND}
end
maki.agent.resolve_model = function() return {{}} end
{INIT}
local native_notify = maki.session.notify
local sent, seen, waiter = 0, 0, nil
maki.session.notify = function(text, opts)
  local ok, err = native_notify(text, opts)
  sent = sent + 1
  if waiter then
    local done = waiter
    waiter = nil
    done()
  end
  return ok, err
end
maki.api.register_tool({{
  name = "{NOTICE_TOOL}",
  description = "wait until the plugin has sent one more notice than last time",
  schema = {{ type = "object", properties = {{}} }},
  audiences = {{ "main" }},
  handler = function()
    if sent == seen then
      maki.async.await(1, function(done) waiter = done end)
    end
    seen = seen + 1
    return "ready"
  end,
}})
maki.api.register_tool({{
  name = "{LIST_TOOL}",
  description = "list test background sessions",
  schema = {{ type = "object", properties = {{}} }},
  audiences = {{ "main" }},
  handler = function(_, ctx)
    return maki.json.encode(maki.agent.list({{ session = ctx:session_id() }}))
  end,
}})
"#
    );
    host.load_source_with_opts(
        PLUGIN,
        &source,
        serde_json::Map::from_iter([
            ("allow_background".into(), json!(true)),
            ("max_concurrent".into(), json!(1)),
        ]),
    )
    .unwrap();
}

fn seed(
    mailbox: &SessionMailbox,
    session: MakiId,
) -> (Arc<AgentSeed>, flume::Receiver<()>, flume::Sender<()>) {
    let seed = mailbox.publish_seed(stub_seed(session));
    let (started, started_rx) = flume::bounded(1);
    let (release, release_rx) = flume::bounded(1);
    seed.model_slot.store(Arc::new(ModelSlot {
        model: seed.model_slot.load().model.clone(),
        provider: Arc::new(ParkedProvider {
            started,
            release: release_rx,
        }),
    }));
    (seed, started_rx, release)
}

fn exec(reg: &ToolRegistry, name: &str, input: Value, ctx: &ToolContext) -> ToolExecResult {
    let inv = reg.get(name).unwrap().tool.parse(&input).unwrap();
    smol::block_on(inv.execute(ctx))
}

fn text(result: ToolExecResult) -> (String, bool) {
    let output = match result.output {
        Ok(output) => output,
        Err(error) => return (error, true),
    };
    let output = match output {
        ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
        other => panic!("unexpected output {other:?}"),
    };
    (output, false)
}

fn start(reg: &ToolRegistry, ctx: &ToolContext, name: &str) {
    let (output, failed) = text(exec(
        reg,
        "task",
        json!({ "description": name, "prompt": PROMPT, "background": true }),
        ctx,
    ));
    assert!(!failed, "{output}");
    assert!(output.starts_with(STARTED), "{output}");
}

fn sessions(reg: &ToolRegistry, ctx: &ToolContext) -> Vec<Value> {
    let (output, failed) = text(exec(reg, LIST_TOOL, json!({}), ctx));
    assert!(!failed, "{output}");
    match serde_json::from_str(&output).unwrap() {
        Value::Array(tasks) => tasks,
        Value::Object(tasks) if tasks.is_empty() => Vec::new(),
        other => panic!("unexpected session list: {other}"),
    }
}

fn session_id<'a>(tasks: &'a [Value], name: &str) -> &'a str {
    tasks
        .iter()
        .find(|task| task["name"] == name)
        .and_then(|task| task["id"].as_str())
        .unwrap()
}

/// The next notice the plugin sends, and whether it woke the chat.
fn next_notice(reg: &ToolRegistry, ctx: &ToolContext, mailbox: &SessionMailbox) -> (String, bool) {
    let (output, failed) = text(exec(reg, NOTICE_TOOL, json!({}), ctx));
    assert!(!failed, "{output}");
    let woken = mailbox.claim_wake();
    let woke = !woken.is_empty();
    let notices = if woke { woken } else { mailbox.drain() };
    assert_eq!(notices.len(), 1);
    (notices[0].first_text_content().unwrap().to_owned(), woke)
}

/// What `maki.task.stop` does: cancel natively, then tell the owner.
fn stop(host: &PluginHost, session: MakiId, id: &str) {
    host.event_handle().cancel_plugin_session(id);
    fire_task_stopped(host, session, id);
}

fn fire_task_stopped(host: &PluginHost, session: MakiId, id: &str) {
    host.event_handle().fire_autocmd(
        "TaskStopped",
        json!({ "id": id, "session_id": session.to_string(), "plugin": PLUGIN }),
    );
}

#[test]
fn background_result_outlives_the_calling_turn_and_wakes_the_chat() {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let (foreground, token) = CancelToken::new();
    let ctx = seed.tool_context(token);
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    foreground.cancel();
    release.send(()).unwrap();
    let (text, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(woke);
    assert!(text.contains(FIRST), "{text}");
    assert!(text.contains(FINISHED), "{text}");
    assert!(text.contains(RESULT), "{text}");
    assert!(sessions(&reg, &ctx).is_empty());
}

#[test]
fn user_stop_of_a_running_task_queues_a_failure_and_frees_its_slot() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    stop(&host, session, session_id(&sessions(&reg, &ctx), FIRST));
    let (text, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(!woke, "user stop woke the chat");
    assert!(text.contains(STOPPED_BY_USER_ERR), "{text}");
    assert!(text.contains(FAILED), "{text}");
    start(&reg, &ctx, SECOND);
    started.recv().unwrap();
    release.send(()).unwrap();
    assert!(next_notice(&reg, &ctx, &mailbox).0.contains(SECOND));
}

#[test]
fn user_stop_of_a_queued_task_skips_the_provider() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    start(&reg, &ctx, SECOND);
    stop(&host, session, session_id(&sessions(&reg, &ctx), SECOND));
    let (text, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(!woke, "user stop woke the chat");
    assert!(text.contains(SECOND), "{text}");
    assert!(text.contains(STOPPED_BY_USER_ERR), "{text}");
    release.send(()).unwrap();
    assert!(next_notice(&reg, &ctx, &mailbox).0.contains(FIRST));
    assert!(started.try_recv().is_err());
    assert!(sessions(&reg, &ctx).is_empty());
}

/// Any plugin can fire `TaskStopped`, so the event alone must not stop work.
#[test]
fn a_spoofed_stop_event_leaves_a_queued_task_alone() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    start(&reg, &ctx, SECOND);
    fire_task_stopped(&host, session, session_id(&sessions(&reg, &ctx), SECOND));
    release.send(()).unwrap();
    assert!(next_notice(&reg, &ctx, &mailbox).0.contains(FIRST));
    started.recv().unwrap();
    release.send(()).unwrap();
    let (text, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(woke);
    assert!(text.contains(SECOND), "{text}");
    assert!(text.contains(RESULT), "{text}");
}

#[test_case(false ; "session_end")]
#[test_case(true ; "reload")]
fn teardown_stops_background_work_without_notifying_a_gone_chat(reload: bool) {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, _release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    start(&reg, &ctx, SECOND);
    if reload {
        host.unload(PLUGIN).unwrap();
        load(&host);
    } else {
        host.event_handle()
            .end_sessions_blocking([session], SessionEndReason::Shutdown);
    }
    assert!(sessions(&reg, &ctx).is_empty());
    assert!(mailbox.drain().is_empty());
    assert!(started.try_recv().is_err());
}

/// Headless, SDK and ACP runs have no live chat to deliver to.
#[test]
fn background_without_a_live_chat_runs_blocking() {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    drop(seed);
    drop(mailbox);
    let (done_tx, done_rx) = flume::bounded(1);
    thread::spawn(move || {
        let result = exec(
            &reg,
            "task",
            json!({ "description": FIRST, "prompt": PROMPT, "background": true }),
            &ctx,
        );
        done_tx.send(text(result)).unwrap();
    });
    started.recv().unwrap();
    assert!(done_rx.try_recv().is_err());
    release.send(()).unwrap();
    assert_eq!(done_rx.recv().unwrap(), (RESULT.into(), false));
}
