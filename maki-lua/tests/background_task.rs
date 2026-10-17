//! Real task plugin and background sessions with a callback-controlled provider.
//! Model selection inherits the test provider; the session APIs stay real.

use std::sync::Arc;
use std::time::Duration;

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

const TIMEOUT: Duration = Duration::from_secs(5);
const RESULT: &str = "background answer";
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
"#
    );
    host.load_source(PLUGIN, &source).unwrap();
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

fn start(reg: &ToolRegistry, ctx: &ToolContext) -> String {
    let (output, failed) = text(exec(
        reg,
        "task",
        json!({
            "description": "test background task", "prompt": PROMPT, "background": true,
        }),
        ctx,
    ));
    assert!(!failed, "{output}");
    let receipt: Value = serde_json::from_str(&output).unwrap();
    receipt["id"].as_str().unwrap().to_owned()
}

fn threaded_result(
    reg: Arc<ToolRegistry>,
    ctx: ToolContext,
    id: String,
) -> flume::Receiver<ToolExecResult> {
    let (tx, rx) = flume::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(exec(&reg, "task_result", json!({ "id": id }), &ctx));
    });
    rx
}

#[test]
fn background_result_outlives_the_calling_turn_and_notifies_once() {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let (foreground, token) = CancelToken::new();
    let ctx = seed.tool_context(token);
    let id = start(&reg, &ctx);
    started.recv_timeout(TIMEOUT).unwrap();
    foreground.cancel();

    let other = stub_seed(MakiId::generate()).tool_context(CancelToken::none());
    let (error, failed) = text(exec(&reg, "task_result", json!({ "id": id }), &other));
    assert!(failed);
    assert!(error.contains("unknown background task"));

    let query = threaded_result(
        Arc::clone(&reg),
        seed.tool_context(CancelToken::none()),
        id.clone(),
    );
    release.send(()).unwrap();
    assert_eq!(
        text(query.recv_timeout(TIMEOUT).unwrap()),
        (RESULT.into(), false)
    );
    assert_eq!(
        text(exec(
            &reg,
            "task_result",
            json!({ "id": id }),
            &seed.tool_context(CancelToken::none())
        )),
        (RESULT.into(), false)
    );
    let notices = mailbox.claim_wake();
    assert_eq!(notices.len(), 1);
    let notice = notices[0].first_text_content().unwrap();
    assert!(notice.contains(&id));
    assert!(notice.contains("done"));
    assert!(!notice.contains(RESULT));
    assert!(mailbox.drain().is_empty());
}

#[test_case::test_case(false ; "explicit_cancel")]
#[test_case::test_case(true ; "session_end")]
fn cancellation_settles_native_background_work(session_end: bool) {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, _release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    let id = start(&reg, &ctx);
    started.recv_timeout(TIMEOUT).unwrap();
    let query = threaded_result(
        Arc::clone(&reg),
        seed.tool_context(CancelToken::none()),
        id.clone(),
    );
    if session_end {
        host.event_handle()
            .end_sessions_blocking([session], SessionEndReason::Shutdown);
        let (error, failed) = text(exec(&reg, "task_result", json!({ "id": id }), &ctx));
        assert!(failed);
        assert!(error.contains("unknown background task"));
        assert!(mailbox.drain().is_empty());
    } else {
        let (_, failed) = text(exec(&reg, "task_cancel", json!({ "id": id }), &ctx));
        assert!(!failed);
    }
    let (output, failed) = text(query.recv_timeout(TIMEOUT).unwrap());
    assert!(failed, "{output}");
    assert!(
        output.contains("cancelled") || output.contains("unknown background task"),
        "{output}"
    );
}

#[test]
fn a_mailbox_without_an_interactive_seed_rejects_background_admission() {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let _mailbox = SessionMailbox::register(session);
    let ctx = stub_seed(session).tool_context(CancelToken::none());
    let (output, failed) = text(exec(
        &reg,
        "task",
        json!({
            "description": "no live loop", "prompt": PROMPT, "background": true,
        }),
        &ctx,
    ));
    assert!(failed);
    assert!(output.contains("session not live"), "{output}");
}

#[test]
fn reload_discards_receipts_and_does_not_reuse_their_ids() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    let old_id = start(&reg, &ctx);
    started.recv_timeout(TIMEOUT).unwrap();
    host.unload(PLUGIN).unwrap();
    load(&host);
    let (error, failed) = text(exec(&reg, "task_result", json!({ "id": old_id }), &ctx));
    assert!(failed);
    assert!(error.contains("unknown background task"));
    let new_id = start(&reg, &ctx);
    assert_ne!(new_id, old_id);
    started.recv_timeout(TIMEOUT).unwrap();
    release.send(()).unwrap();
    let query = threaded_result(Arc::clone(&reg), ctx, new_id);
    assert_eq!(
        text(query.recv_timeout(TIMEOUT).unwrap()),
        (RESULT.into(), false)
    );
}
