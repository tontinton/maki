//! Moving a running call to the background through a real agent loop: a
//! scripted model, the bundled `task_stop`, and a Lua tool that registers a
//! move hook. Each scenario follows the move from the Ctrl+B request to what
//! the model reads and what the transcript shows.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use maki_agent::permissions::PermissionManager;
use maki_agent::session::Resumed;
use maki_agent::tools::ToolRegistry;
use maki_agent::{
    AgentConfig, AgentEvent, AgentInput, AgentMode, DoneReason, InputSource, McpConfigErrors,
    ModelSlot, ToolDoneEvent, ToolOutputLines,
};
use maki_config::{ModelPolicy, PermissionsConfig, PluginsConfig, ProjectConfig};
use maki_lua::{MoveOutcome, PluginHost, UiAction};
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::{
    AgentError, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, ThinkingConfig, Timeouts, TokenUsage,
};
use maki_storage::id::SessionRef;
use serde_json::{Value, json};
use tokio::runtime::EnterGuard;

use crate::agent::AgentHandles;
use crate::agent::shared_queue::{QueueItem, QueuedInput};
use crate::app::tests::enter_runtime;

use super::push_prompt;

const WAIT: Duration = Duration::from_secs(30);
const PLUGIN: &str = "background_e2e";
const PARK_TOOL: &str = "bg_park";
const NOOP_TOOL: &str = "bg_noop";
const STOP_TOOL: &str = "task_stop";
const PARK_ID: &str = "toolu_park";
const NOOP_ID: &str = "toolu_noop";
const STOP_ID: &str = "toolu_stop";
const FIRST_RUN: u64 = 1;
const SECOND_RUN: u64 = 2;
const BUILD_PROMPT: &str = "run the build";
const NEXT_PROMPT: &str = "what happened to the build?";
const WAITING_TEXT: &str = "the build runs in the background";
const PASSED_TEXT: &str = "the build passed";
const RECEIPT: &str = "Running in the background";
const SUMMARY: &str = "build tests";
const FINAL_OUTPUT: &str = "all tests passed";
const PARTIAL_OUTPUT: &str = "partial output so far";
const NOOP_OUTPUT: &str = "noop done";
/// What the parker's handler ends with when nothing cancelled it.
const FINAL_REPLY: &str = "all tests passed:false";
const SCRIPT_EXHAUSTED: &str = "the model was asked for more turns than the test scripted";
const SCRIPTED_PROVIDER: &str = "scripted-provider";
const HOST_STARTS: &str = "the plugin host starts";
const PLUGINS_LOAD: &str = "the bundled plugins and the test tools load";
const NO_REQUEST: &str = "the model was never asked";
const NO_EVENT: &str = "the agent never reported the event the test waits for";
const NO_OUTCOME: &str = "the move request was never answered";
const NO_TASK_EVENT: &str = "the moved call never delivered its final reply";
const ROW_MISSING: &str = "the call never finished its row";
const GATE_CLOSED: &str = "the scripted model stopped waiting for its release";
const RECEIPT_MISSING: &str = "the model's next request carries no result for the moved call";
const SERIALIZES: &str = "messages serialize";
const NO_WAKE: &str = "the finished task left no wake for the chat";
const NOT_IDLE: &str = "check for extra requests only once the loop reported it is idle";
const EXTRA_REQUEST: &str = "an extra request went out";

// Mirrors of the wording in maki-lua's runtime and registry.
const MOVED_BY_USER: &str = "Moved to the background by the user";
const FINISHED: &str = "finished";
const CANCELLED: &str = "cancelled";

/// The bundled `task_stop` and these tools register in the process-wide
/// registry the agent loop reads, so two hosts must not overlap in one
/// process.
static SERIAL: Mutex<()> = Mutex::new(());

/// `bg_park` stands in for a long command: it can move, and it ends when the
/// test clicks its buffer. Like bash it returns at once and keeps a job, so a
/// cancel finishes it with partial output without waiting out the abandon
/// window a parked handler would.
const PLUGIN_SRC: &str = r#"
maki.api.register_tool({
  name = "bg_park",
  description = "waits for a click and can move to the background",
  schema = {
    type = "object",
    properties = {
      receipt = { type = "string" },
      final = { type = "string" },
      partial = { type = "string" },
      summary = { type = "string" },
    },
  },
  audiences = { "main" },
  header = function(input)
    return input.summary
  end,
  handler = function(input, ctx)
    local buf = maki.ui.buf()
    buf:on("click", function()
      ctx:finish(input.final .. ":" .. tostring(ctx:cancelled()))
    end)
    ctx:live_buf(buf)
    ctx:on_background(function()
      return input.receipt
    end)
    maki.async.on_cancel(function()
      ctx:finish({ llm_output = input.partial })
    end)
    maki.fn.jobstart("sleep 600", {})
    return nil
  end,
})

maki.api.register_tool({
  name = "bg_noop",
  description = "returns at once",
  schema = {
    type = "object",
    properties = { output = { type = "string" } },
  },
  audiences = { "main" },
  handler = function(input)
    return input.output
  end,
})
"#;

struct SeenRequest {
    system: String,
    tools: Value,
    messages: Vec<Message>,
}

impl SeenRequest {
    fn serialized(&self) -> Vec<Value> {
        self.messages
            .iter()
            .map(|message| serde_json::to_value(message).expect(SERIALIZES))
            .collect()
    }

    fn observations(&self) -> Vec<&Message> {
        self.messages
            .iter()
            .filter(|message| message.is_observation())
            .collect()
    }

    fn tool_result(&self, id: &str) -> Option<(&str, bool)> {
        self.messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } if tool_use_id == id => Some((content.as_str(), *is_error)),
                _ => None,
            })
    }
}

fn holds_tool_result(message: &Message, id: &str) -> bool {
    message.content.iter().any(
        |block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
    )
}

/// The contract that keeps the cache and every thinking block valid.
#[track_caller]
fn assert_append_only(prev: &SeenRequest, next: &SeenRequest) {
    assert_eq!(prev.system, next.system, "system changed");
    let (prev_tools, next_tools) = (
        prev.tools.as_array().expect("tools are a list"),
        next.tools.as_array().expect("tools are a list"),
    );
    assert!(next_tools.starts_with(prev_tools), "tools were edited");
    assert!(
        next.serialized().starts_with(&prev.serialized()),
        "earlier messages were edited"
    );
}

struct Step {
    response: StreamResponse,
    hold: Option<flume::Receiver<()>>,
}

impl Step {
    fn new(response: StreamResponse) -> Self {
        Self {
            response,
            hold: None,
        }
    }

    /// The model answers only once the test sends on the paired sender.
    fn held(mut self, hold: flume::Receiver<()>) -> Self {
        self.hold = Some(hold);
        self
    }
}

fn response(content: Vec<ContentBlock>, stop_reason: StopReason) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(stop_reason),
        ..Default::default()
    }
}

fn answer(text: &str) -> StreamResponse {
    response(
        vec![ContentBlock::Text { text: text.into() }],
        StopReason::EndTurn,
    )
}

fn call(id: &str, tool: &str, input: Value) -> StreamResponse {
    response(
        vec![ContentBlock::tool_use(id, tool, input)],
        StopReason::ToolUse,
    )
}

fn park_call() -> StreamResponse {
    call(
        PARK_ID,
        PARK_TOOL,
        json!({
            "receipt": RECEIPT,
            "final": FINAL_OUTPUT,
            "partial": PARTIAL_OUTPUT,
            "summary": SUMMARY,
        }),
    )
}

fn noop_call() -> StreamResponse {
    call(NOOP_ID, NOOP_TOOL, json!({ "output": NOOP_OUTPUT }))
}

fn stop_call() -> StreamResponse {
    call(STOP_ID, STOP_TOOL, json!({ "id": PARK_ID }))
}

struct ScriptedProvider {
    steps: Mutex<VecDeque<Step>>,
    requests: flume::Sender<SeenRequest>,
}

impl Provider for ScriptedProvider {
    fn stream_message<'a>(
        &'a self,
        _model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        _event_tx: &'a flume::Sender<ProviderEvent>,
        _opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        let _ = self.requests.send(SeenRequest {
            system: system.to_owned(),
            tools: tools.clone(),
            messages: messages.to_vec(),
        });
        let step = self
            .steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        Box::pin(async move {
            let Some(step) = step else {
                return Err(AgentError::Tool {
                    tool: SCRIPTED_PROVIDER.into(),
                    message: SCRIPT_EXHAUSTED.into(),
                });
            };
            if let Some(hold) = step.hold {
                let _ = hold.recv_async().await;
            }
            Ok(step.response)
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// The notice the model reads when `output` ended the task.
fn expected_text(output: &str) -> String {
    format!("Background {PARK_TOOL} {PARK_ID} ({SUMMARY}) {FINISHED}:\n\n{output}")
}

/// The transcript line. A task stopped by the user reads `cancelled`.
fn expected_display(outcome: &str) -> String {
    format!("Background {PARK_TOOL} {outcome}: {SUMMARY}")
}

struct World {
    handles: AgentHandles,
    host: PluginHost,
    ui: flume::Receiver<UiAction>,
    requests: flume::Receiver<SeenRequest>,
    seen: Vec<AgentEvent>,
    _serial: MutexGuard<'static, ()>,
    _runtime: EnterGuard<'static>,
}

impl World {
    fn start(steps: Vec<Step>) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let runtime = enter_runtime();
        let mut host = PluginHost::new(Arc::clone(ToolRegistry::global_arc())).expect(HOST_STARTS);
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
            .and_then(|()| host.load_source(PLUGIN, PLUGIN_SRC))
            .expect(PLUGINS_LOAD);
        host.ui_attachment().attach();
        let ui = host.ui_action_rx();

        let (seen_requests, requests) = flume::unbounded();
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            steps: Mutex::new(steps.into()),
            requests: seen_requests,
        });
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: crate::components::test_model(),
            provider,
        }));
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            PathBuf::from("/tmp"),
            ProjectConfig::for_project(Path::new("/tmp")),
            Arc::default(),
        ));
        let handles = AgentHandles::spawn(
            &model_slot,
            Resumed::fresh(),
            AgentConfig::default(),
            ToolOutputLines::default(),
            &permissions,
            Timeouts::default(),
            host.event_handle(),
            None,
            McpConfigErrors::new(PathBuf::new()),
            Arc::new(ModelPolicy::default()),
        );
        Self {
            handles,
            host,
            ui,
            requests,
            seen: Vec::new(),
            _serial: serial,
            _runtime: runtime,
        }
    }

    fn prompt(&self, run_id: u64, text: &str) {
        push_prompt(&self.handles, run_id, text);
    }

    /// What the event loop does for an idle chat whose mailbox holds a wake:
    /// claim it and start a run with the notices as its preamble.
    fn claim_wake(&self) -> Vec<Message> {
        self.handles.claim_mailbox_wake()
    }

    fn start_wake_run(&self, run_id: u64, preamble: Vec<Message>) {
        assert!(!preamble.is_empty(), "{NO_WAKE}");
        self.handles.queue.push(QueueItem::Message(QueuedInput {
            text: String::new(),
            input: AgentInput {
                message: String::new(),
                mode: AgentMode::default(),
                images: Vec::new(),
                preamble,
                earlier: Vec::new(),
                thinking: ThinkingConfig::default(),
                fast: false,
                workflow: false,
                prompt: None,
                source: InputSource::Tui,
            },
            run_id,
            displayed: true,
        }));
    }

    fn wait_for(&mut self, is_wanted: impl Fn(&AgentEvent) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            let envelope = self
                .handles
                .agent_rx
                .recv_deadline(deadline)
                .expect(NO_EVENT);
            let wanted = is_wanted(&envelope.event);
            self.seen.push(envelope.event);
            if wanted {
                return;
            }
        }
    }

    /// The loop reports it has no run left once the run's own cleanup is done.
    fn wait_idle(&mut self) {
        self.wait_for(|event| matches!(event, AgentEvent::QueueDrained));
    }

    /// Ctrl+B for the call `id`, once it announced it can move.
    fn move_call(&mut self, id: &str) {
        self.wait_for(
            |event| matches!(event, AgentEvent::ToolMovable { id: movable, movable: true } if movable == id),
        );
        let outcome = self
            .host
            .event_handle()
            .request_background(id.to_owned())
            .recv_timeout(WAIT)
            .expect(NO_OUTCOME);
        assert_eq!(outcome, MoveOutcome::Moved);
    }

    fn next_request(&self) -> SeenRequest {
        self.requests.recv_timeout(WAIT).expect(NO_REQUEST)
    }

    /// The provider records a request before it answers, and `QueueDrained`
    /// follows the last answer of the loop's last run, so once that event is
    /// read nothing can still be in flight and `try_recv` is exact. Only the
    /// event loop starts runs, never this one on its own, so no later run
    /// needs a point to wait for.
    #[track_caller]
    fn assert_no_more_requests(&self) {
        assert!(
            matches!(self.seen.last(), Some(AgentEvent::QueueDrained)),
            "{NOT_IDLE}"
        );
        assert!(self.requests.try_recv().is_err(), "{EXTRA_REQUEST}");
    }

    fn finish_moved_call(&self) {
        self.host
            .event_handle()
            .request_click(PARK_ID.to_owned(), 0);
    }

    /// The final reply of the moved call, as its task tab gets it. The chat
    /// was told before this is sent.
    fn task_done(&self) -> ToolDoneEvent {
        let deadline = Instant::now() + WAIT;
        loop {
            let action = self.ui.recv_deadline(deadline).expect(NO_TASK_EVENT);
            if let UiAction::PluginTaskEvent { envelope, .. } = action
                && let AgentEvent::ToolDone(done) = envelope.event
            {
                return *done;
            }
        }
    }

    fn done_reasons(&self) -> Vec<DoneReason> {
        self.seen
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Done { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect()
    }

    fn observation_lines(&self) -> Vec<(&str, Option<&str>)> {
        self.seen
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ObservationLine { text, detail, .. } => {
                    Some((text.as_str(), detail.as_deref()))
                }
                _ => None,
            })
            .collect()
    }

    fn errors(&self) -> Vec<&str> {
        self.seen
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Error { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The transcript row of the moved call carries the receipt and knows it
    /// moved. Only events already read are searched, so call it after
    /// `wait_idle`.
    #[track_caller]
    fn assert_row_moved(&self) {
        let done = self
            .seen
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolDone(done) if done.id == PARK_ID => Some(done),
                _ => None,
            })
            .expect(ROW_MISSING);
        assert!(done.backgrounded);
    }

    /// The model's view of a move: the receipt answers the call, names the
    /// task and says how to stop it.
    #[track_caller]
    fn assert_receipt(request: &SeenRequest) {
        let (receipt, is_error) = request.tool_result(PARK_ID).expect(RECEIPT_MISSING);
        assert!(!is_error, "{receipt}");
        for fragment in [RECEIPT, MOVED_BY_USER, PARK_ID, STOP_TOOL] {
            assert!(
                receipt.contains(fragment),
                "{fragment} missing from {receipt}"
            );
        }
    }

    #[track_caller]
    fn assert_no_wake_left(&self) {
        assert!(self.claim_wake().is_empty());
    }
}

#[test]
fn a_moved_call_whose_turn_ends_wakes_the_chat_with_its_result() {
    let mut world = World::start(vec![
        Step::new(park_call()),
        Step::new(answer(WAITING_TEXT)),
        Step::new(answer(PASSED_TEXT)),
    ]);
    world.prompt(FIRST_RUN, BUILD_PROMPT);
    world.move_call(PARK_ID);
    let first = world.next_request();
    let receipt_request = world.next_request();
    World::assert_receipt(&receipt_request);
    world.wait_idle();
    world.assert_row_moved();
    assert_eq!(world.done_reasons(), [DoneReason::EndTurn]);
    world.assert_no_wake_left();

    world.finish_moved_call();
    assert_eq!(world.task_done().output.as_text(), FINAL_REPLY);
    let preamble = world.claim_wake();
    assert_eq!(preamble.len(), 1);
    let notice = &preamble[0];
    let text = expected_text(FINAL_REPLY);
    let display = expected_display(FINISHED);
    assert_eq!(notice.first_text_content(), Some(text.as_str()));
    assert_eq!(notice.display_text.as_deref(), Some(display.as_str()));

    world.start_wake_run(SECOND_RUN, preamble);
    let wake_request = world.next_request();
    world.wait_idle();

    let observations = wake_request.observations();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].first_text_content(), Some(text.as_str()));
    assert_append_only(&first, &receipt_request);
    assert_append_only(&receipt_request, &wake_request);
    assert_eq!(
        world.observation_lines(),
        [(display.as_str(), Some(FINAL_REPLY))]
    );
    assert_eq!(
        world.done_reasons(),
        [DoneReason::EndTurn, DoneReason::EndTurn]
    );
    assert!(world.errors().is_empty(), "{:?}", world.errors());
    world.assert_no_more_requests();
}

#[test]
fn a_result_landing_mid_run_follows_the_next_tool_results_without_a_new_request() {
    let (release_model, model_gate) = flume::unbounded::<()>();
    let mut world = World::start(vec![
        Step::new(park_call()),
        Step::new(noop_call()).held(model_gate),
        Step::new(answer(PASSED_TEXT)),
    ]);
    world.prompt(FIRST_RUN, BUILD_PROMPT);
    world.move_call(PARK_ID);
    let first = world.next_request();
    let receipt_request = world.next_request();
    World::assert_receipt(&receipt_request);

    world.finish_moved_call();
    assert_eq!(world.task_done().output.as_text(), FINAL_REPLY);
    release_model.send(()).expect(GATE_CLOSED);
    let after_results = world.next_request();
    world.wait_idle();
    world.assert_row_moved();

    let text = expected_text(FINAL_REPLY);
    let observation_at = after_results
        .messages
        .iter()
        .position(Message::is_observation)
        .expect("the notice reached the model");
    assert_eq!(
        after_results.messages[observation_at].first_text_content(),
        Some(text.as_str())
    );
    assert!(
        holds_tool_result(&after_results.messages[observation_at - 1], NOOP_ID),
        "the notice follows the tool results of the round"
    );
    assert_eq!(after_results.observations().len(), 1);
    let (noop_result, _) = after_results.tool_result(NOOP_ID).expect(RECEIPT_MISSING);
    assert!(noop_result.contains(NOOP_OUTPUT), "{noop_result}");
    assert_append_only(&first, &receipt_request);
    assert_append_only(&receipt_request, &after_results);
    world.assert_no_more_requests();

    let display = expected_display(FINISHED);
    assert_eq!(
        world.observation_lines(),
        [(display.as_str(), Some(FINAL_REPLY))]
    );
    assert_eq!(world.done_reasons(), [DoneReason::EndTurn]);
    assert!(world.errors().is_empty(), "{:?}", world.errors());
    world.assert_no_wake_left();
}

#[test]
fn a_task_stopped_by_the_user_is_a_quiet_notice_that_waits_for_the_next_prompt() {
    let mut world = World::start(vec![
        Step::new(park_call()),
        Step::new(answer(WAITING_TEXT)),
        Step::new(answer(PASSED_TEXT)),
    ]);
    world.prompt(FIRST_RUN, BUILD_PROMPT);
    world.move_call(PARK_ID);
    let _ = world.next_request();
    let receipt_request = world.next_request();
    World::assert_receipt(&receipt_request);
    world.wait_idle();
    world.assert_row_moved();

    world.host.event_handle().cancel_plugin_session(PARK_ID);
    assert_eq!(world.task_done().output.as_text(), PARTIAL_OUTPUT);
    world.assert_no_wake_left();

    world.prompt(SECOND_RUN, NEXT_PROMPT);
    let next_request = world.next_request();
    world.wait_idle();

    let text = expected_text(PARTIAL_OUTPUT);
    let display = expected_display(CANCELLED);
    let observations = next_request.observations();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].first_text_content(), Some(text.as_str()));
    assert_eq!(
        observations[0].display_text.as_deref(),
        Some(display.as_str())
    );
    assert_append_only(&receipt_request, &next_request);
    assert_eq!(
        world.observation_lines(),
        [(display.as_str(), Some(PARTIAL_OUTPUT))]
    );
    assert!(world.errors().is_empty(), "{:?}", world.errors());
}

#[test]
fn a_task_stop_by_the_model_returns_the_partial_output_and_sends_no_notice() {
    let mut world = World::start(vec![
        Step::new(park_call()),
        Step::new(stop_call()),
        Step::new(answer(PASSED_TEXT)),
    ]);
    world.prompt(FIRST_RUN, BUILD_PROMPT);
    world.move_call(PARK_ID);
    let _ = world.next_request();
    let receipt_request = world.next_request();
    World::assert_receipt(&receipt_request);
    let after_stop = world.next_request();
    world.wait_idle();
    world.assert_row_moved();

    let (stopped, is_error) = after_stop
        .tool_result(STOP_ID)
        .expect("the model got an answer to task_stop");
    assert!(!is_error, "{stopped}");
    assert!(stopped.contains(PARTIAL_OUTPUT), "{stopped}");
    assert_eq!(world.task_done().output.as_text(), PARTIAL_OUTPUT);

    assert!(after_stop.observations().is_empty());
    assert!(world.observation_lines().is_empty());
    assert!(world.handles.mailbox.drain().is_empty());
    assert_append_only(&receipt_request, &after_stop);
    assert_eq!(world.done_reasons(), [DoneReason::EndTurn]);
    assert!(world.errors().is_empty(), "{:?}", world.errors());
    world.assert_no_more_requests();
}

/// Esc does not touch moved work, which keeps running and reports like any
/// other.
#[test]
fn esc_leaves_moved_work_running() {
    let (_model_gate_open, model_gate) = flume::unbounded::<()>();
    let mut world = World::start(vec![
        Step::new(park_call()),
        Step::new(answer(WAITING_TEXT)).held(model_gate),
    ]);
    world.prompt(FIRST_RUN, BUILD_PROMPT);
    world.move_call(PARK_ID);
    let _ = world.next_request();
    let receipt_request = world.next_request();
    World::assert_receipt(&receipt_request);
    world.handles.cancel_run(FIRST_RUN);
    world.wait_idle();
    world.assert_row_moved();
    assert_eq!(world.done_reasons(), [DoneReason::Cancelled]);
    world.assert_no_wake_left();

    world.finish_moved_call();
    assert_eq!(world.task_done().output.as_text(), FINAL_REPLY);
    let preamble = world.claim_wake();
    assert_eq!(preamble.len(), 1);
    let text = expected_text(FINAL_REPLY);
    assert_eq!(preamble[0].first_text_content(), Some(text.as_str()));
    world.assert_no_more_requests();
}
