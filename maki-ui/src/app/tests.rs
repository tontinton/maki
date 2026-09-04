use super::*;
use crate::agent::shared_queue;
use crate::chat::{CANCELLED_TEXT, DONE_TEXT, ERROR_TEXT};
use crate::components::btw_modal::BtwEvent;
use crate::components::command::ParsedCommand;
use crate::components::file_picker::UNREADABLE_DIR_MSG;
use crate::components::keybindings::{KeybindContext, key as kb};
use crate::components::messages::ScrollPos;
use crate::components::rewind_picker::RewindEntry;
use crate::components::{ExitRequest, buffer_text, key, test_model};
use crate::repaint::expect::{OWED, QUIET};
use crate::selection::{RowPos, SelectableZone, SelectionState, SelectionZone};
use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    DoneReason, ImageMediaType, McpConfigErrors, McpServerInfo, McpServerStatus, McpSnapshot,
    McpSnapshotReader, ToolDoneEvent, ToolOutput, ToolStartEvent, TurnCompleteEvent,
};
use maki_config::{Effect, PermissionRule, PermissionsConfig, ToolKey, UiConfig};
use maki_lua::test_support::{HintWriterHandle, hint_writer_pair};
use maki_lua::{
    BuiltinAction, HintReader, KeymapReader, LuaCommandInfo, LuaCommandReader, PackCommand,
    PackPlan, PackPreparation, PackReport, PlanActionReader, SessionEndReason,
};
use maki_providers::{ContentBlock, Effort, Message, Role, THINKING_USAGE, TokenUsage};
use maki_storage::sessions::{SessionMeta, StoredMode, StoredThinking};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;

const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const TASK_ID: &str = "task1";
const PACKUPDATE: &str = "/packupdate";
const PACK_NAME: &str = "demo";
const PACKDEL_USAGE: &str = "/packdel: name a package, or pass ++all";
const PACK_FAILURES: &str = "first; second";
const PACK_REVIEW_PROMPT: &str = "Apply these package changes?";
pub(crate) const RESEARCH_NAME: &str = "research";
const SUB_TOOL_ID: &str = "sub_t1";
const TOOL_OUTPUT_LINE: &str = "hello from the subagent";
const LATE_MODEL_SPEC: &str = "zai/glm-5";
const HINT_PLUGIN: &str = "statusline";
const HINT_TEXT: &str = "2/4 staged";
const HINT_STYLE: &str = "fg";
const RETRY_MESSAGE: &str = "overloaded";
const RETRY_DELAY: Duration = Duration::from_secs(5);
const MISSING_DIR: &str = "gone";
const RESUMED_PROMPT: &str = "carry me over";
const SONNET_SPEC: &str = "anthropic/claude-sonnet-4-5";
const OPUS_SPEC: &str = "anthropic/claude-opus-4-8";
const PLAIN_MODEL_SPEC: &str = "ollama/qwen3";
const MODEL_CHANGED_EVENT: &str = "ModelChanged";
const PLAN_READY_EVENT: &str = "PlanReady";
const PLAN_DRAFT_PATH: &str = "/tmp/plan.md";
const WALK_TIMEOUT: Duration = Duration::from_secs(5);
const CURSOR_STAYS_HIDDEN: &str = "the hardware cursor must never be shown";
const CURSOR_ON_SCREEN: &str = "the reported cursor must be on screen";
const CURSOR_ON_REVERSED_CELL: &str = "the focused input box owns a reversed cursor cell";
const OVERLAY_TAKES_THE_CURSOR: &str = "an overlay unfocuses the input box, so no cell is reversed";
/// Stands in for a size the provider measured, baseline included.
const MEASURED_CONTEXT: u32 = 100_000;
const TEST_MODEL_SPEC: &str = "test-model";
const TEST_CWD: &str = "/tmp/test";
const PERMISSIONS_CWD: &str = "/tmp";
/// The rewind fixture holds a few dozen bytes of chat, far below this, so it
/// doubles as the window the gauge is allowed to land in.
const SMALL_HISTORY: u32 = 1_000;

fn set_zone(app: &mut App, zone: SelectionZone, area: Rect) {
    app.zones.push(SelectableZone { area, zone });
}

fn build_app(dir: StateDir, writer: Arc<StorageWriter>) -> App {
    build_app_with_lua(dir, writer, LuaCommandReader::empty())
}

fn build_app_with_lua(
    dir: StateDir,
    writer: Arc<StorageWriter>,
    lua_commands: LuaCommandReader,
) -> App {
    build_app_with_session(
        dir,
        writer,
        lua_commands,
        AppSession::new(TEST_MODEL_SPEC, TEST_CWD),
        test_permissions(false),
    )
}

fn test_permissions(yolo: bool) -> Arc<PermissionManager> {
    Arc::new(PermissionManager::new(
        PermissionsConfig {
            yolo,
            ..Default::default()
        },
        PathBuf::from(PERMISSIONS_CWD),
        Arc::default(),
    ))
}

fn build_app_with_session(
    dir: StateDir,
    writer: Arc<StorageWriter>,
    lua_commands: LuaCommandReader,
    session: AppSession,
    permissions: Arc<PermissionManager>,
) -> App {
    let model = test_model();
    App::new(
        &model,
        session,
        dir,
        Arc::new(ArcSwapOption::empty()),
        McpSnapshotReader::empty(),
        McpConfigErrors::new(PathBuf::new()),
        lua_commands,
        KeymapReader::empty(),
        HintReader::empty(),
        PlanActionReader::empty(),
        writer,
        UiConfig::default(),
        100,
        permissions,
        Arc::from([]),
        maki_lua::EventHandle::disconnected_for_test(),
        Arc::new(maki_config::ModelPolicy::default()),
    )
}

fn test_writer(dir: StateDir) -> StorageWriter {
    StorageWriter::new(dir, flume::unbounded().0)
}

pub(crate) fn test_app() -> App {
    spawned_app(
        AppSession::new(TEST_MODEL_SPEC, TEST_CWD),
        test_permissions(false),
    )
}

/// A tab the way `Ctrl-N` and a resume build one. `App::new` takes the session
/// plus a fork of the prototype manager, and everything the permissions do has
/// to come back out of that meta.
fn spawned_app(session: AppSession, permissions: Arc<PermissionManager>) -> App {
    let dir = StateDir::from_path(env::temp_dir());
    let writer = Arc::new(test_writer(dir.clone()));
    let mut app =
        build_app_with_session(dir, writer, LuaCommandReader::empty(), session, permissions);
    let (shared_queue, _rx) = shared_queue::queue();
    app.queue.set_shared(shared_queue);
    app
}

/// A `test_app` past its idle splash, whose drifting starfield would mask
/// every other cadence.
fn app_without_splash() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta { text: "hi".into() }));
    app.update(done_event());
    app
}

/// Hands back the slot providers publish their model lists into, since the app
/// keeps no handle to it once the picker owns it.
fn app_with_model_slot() -> (App, Arc<ArcSwapOption<Vec<String>>>) {
    let models = Arc::new(ArcSwapOption::empty());
    let mut app = test_app();
    app.model_picker = ModelPicker::new(Arc::clone(&models));
    (app, models)
}

/// Hands back the end a plugin publishes hints through. That is the Lua thread
/// in production, and this test here. Seeding the watch from the new reader is
/// what `App::new` does, and skipping it would make the first poll report the
/// swap itself.
fn app_with_hints() -> (App, HintWriterHandle) {
    let (writer, reader) = hint_writer_pair();
    let mut app = test_app();
    app.hints = Watch::seeded(reader.load_full());
    app.hint_reader = reader;
    (app, writer)
}

fn tempdir_app() -> (TempDir, StateDir, Arc<StorageWriter>, App) {
    let tmp = TempDir::new().unwrap();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    let writer = Arc::new(test_writer(dir.clone()));
    let app = build_app(dir.clone(), Arc::clone(&writer));
    (tmp, dir, writer, app)
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Msg {
    Msg::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn agent_msg(event: AgentEvent) -> Msg {
    agent_msg_with_run_id(event, 1)
}

fn agent_msg_with_run_id(event: AgentEvent, run_id: u64) -> Msg {
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: None,
        run_id,
    }))
}

fn done() -> AgentEvent {
    AgentEvent::Done {
        usage: TokenUsage::default(),
        cost: None,
        list_cost: None,
        context_size: 0,
        context_window: 0,
        num_turns: 1,
        reason: DoneReason::EndTurn,
    }
}

fn done_event() -> Msg {
    agent_msg(done())
}

pub(crate) fn end_turn(app: &mut App) {
    app.update(done_event());
}

fn subagent_info(parent_id: &str, name: &str) -> SubagentInfo {
    subagent_info_with_tx(parent_id, name, None)
}

fn subagent_info_with_tx(
    parent_id: &str,
    name: &str,
    answer_tx: Option<flume::Sender<String>>,
) -> SubagentInfo {
    SubagentInfo {
        parent_tool_use_id: parent_id.into(),
        name: name.into(),
        prompt: None,
        model: None,
        answer_tx,
    }
}

fn subagent_msg(event: AgentEvent, parent_id: &str, name: Option<&str>) -> Msg {
    subagent_msg_with_run_id(event, parent_id, name, 1)
}

fn subagent_msg_with_run_id(
    event: AgentEvent,
    parent_id: &str,
    name: Option<&str>,
    run_id: u64,
) -> Msg {
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(subagent_info(parent_id, name.unwrap_or("Agent"))),
        run_id,
    }))
}

fn subagent_msg_with_prompt(
    event: AgentEvent,
    parent_id: &str,
    name: Option<&str>,
    prompt: Option<&str>,
) -> Msg {
    let mut info = subagent_info(parent_id, name.unwrap_or("Agent"));
    info.prompt = prompt.map(String::from);
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(info),
        run_id: 1,
    }))
}

fn subagent_msg_with_model(event: AgentEvent, parent_id: &str, name: &str, model: &str) -> Msg {
    let mut info = subagent_info(parent_id, name);
    info.model = Some(model.into());
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(info),
        run_id: 1,
    }))
}

fn tool_start(id: &str, tool: &str) -> AgentEvent {
    AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: id.into(),
        tool: tool.into(),
        summary: id.into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))
}

fn turn_complete(usage: TokenUsage, model: &str, cost: Option<f64>) -> AgentEvent {
    AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: Default::default(),
        usage,
        model: model.into(),
        cost,
        list_cost: None,
        context_size: None,
        context_window: 0,
    }))
}

fn tool_results_submitted() -> AgentEvent {
    AgentEvent::ToolResultsSubmitted {
        message: Box::new(Message::user(String::new())),
    }
}

#[test]
fn typing_and_submit() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::SendMessage(s) if s.message == "hi"));
    assert_eq!(app.status, Status::Streaming);
    // Regression check: the bubble has to be on screen the same frame we
    // submit, otherwise it briefly sits one row too high before snapping down.
    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::User),
    );
    assert_eq!(app.main_chat().last_message_text(), "hi");
}

#[test]
fn mailbox_wake_starts_without_an_empty_user_bubble() {
    let mut app = test_app();
    let actions = app.start_mailbox_run(vec![Message::observation("failed".into())]);

    assert!(matches!(
        &actions[..],
        [Action::SendMessage(input)]
            if input.message.is_empty()
                && input.preamble.len() == 1
                && input.preamble[0].is_observation()
    ));
    assert_eq!(app.status, Status::Streaming);
    assert!(app.main_chat().segment_search_texts().is_empty());
}

fn with_text(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
}

fn with_image(app: &mut App) {
    let img = ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="));
    app.input_box.attach_image(img);
}

#[test_case(with_text as fn(&mut App)  ; "clears_text")]
#[test_case(with_image as fn(&mut App) ; "clears_image")]
fn ctrl_c_clears_nonempty_input(setup: fn(&mut App)) {
    let mut app = test_app();
    setup(&mut app);
    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.input_box.is_empty());
}

#[test]
fn ctrl_c_quits_when_input_empty() {
    let mut app = test_app();
    app.status = Status::Idle;
    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(matches!(actions.as_slice(), [Action::ManualExit]));
}

#[test_case(done(), ExitRequest::Success ; "done_exits_success")]
#[test_case(AgentEvent::Error { message: "boom".into() }, ExitRequest::Error ; "error_exits_error")]
fn exit_on_done_flag_triggers_exit(event: AgentEvent, expected: ExitRequest) {
    let mut app = test_app();
    app.exit_on_done = true;
    app.status = Status::Streaming;
    app.run_id = 1;
    let actions = app.update(agent_msg(event));
    assert_eq!(app.exit_request, expected);
    assert!(actions.is_empty());
}

#[test]
fn reset_session_clears_exit_request_source() {
    let mut app = test_app();
    app.exit_on_done = true;
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        cost: None,
        list_cost: None,
        context_size: 0,
        context_window: 0,
        num_turns: 1,
        reason: DoneReason::EndTurn,
    }));

    app.reset_session();

    assert_eq!(app.exit_request, ExitRequest::None);
}

#[test]
fn toggle_mode_state_machine() {
    let tab = |app: &mut App| app.update(Msg::Key(key(KeyCode::Tab)));

    let mut app = test_app();
    assert_eq!(app.state.mode, Mode::Build);

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    let first_path = app.state.plan.path().unwrap().to_path_buf();
    assert!(first_path.to_str().unwrap().contains("plans"));

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Build);
    assert!(!app.state.plan.is_ready());

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.state.plan.path().unwrap(), first_path);

    app.state.plan.mark_ready();
    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Build);
    assert!(app.state.plan.is_ready());
    assert_eq!(app.state.plan.path().unwrap(), first_path);

    app.state.mode = Mode::Build;
    app.status = Status::Streaming;
    app.run_id = 1;
    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.state.plan.path().unwrap(), first_path);
}

#[test_case(ToolOutput::Plain("wrote 100 bytes to /tmp/plans/test.md".into()), Some("/tmp/plans/test.md".into()), true  ; "write_matching")]
#[test_case(ToolOutput::Diff { path: "/tmp/plans/test.md".into(), before: String::new(), after: String::new(), summary: String::new() }, None, true  ; "edit_matching")]
#[test_case(ToolOutput::Plain("wrote 100 bytes to /tmp/other.rs".into()), Some("/tmp/other.rs".into()), false ; "write_non_matching")]
fn tool_done_transitions_plan_to_ready(
    output: ToolOutput,
    written_path: Option<String>,
    expect_ready: bool,
) {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("/tmp/plans/test.md"));
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output,
        is_error: false,
        annotation: None,
        written_path,
    }))));

    assert_eq!(app.state.plan.is_ready(), expect_ready);
}

#[test]
fn altgr_chars_not_swallowed_by_ctrl_handler() {
    let mut app = test_app();
    let altgr_backslash = KeyEvent {
        code: KeyCode::Char('\\'),
        modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app.update(Msg::Key(altgr_backslash));
    assert_eq!(app.input_box.buffer.value(), "hi\\");
}

#[test_case(Status::Idle      ; "idle")]
#[test_case(Status::Streaming ; "streaming")]
fn paste_works_regardless_of_status(status: Status) {
    let mut app = test_app();
    app.status = status;
    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), "pasted");
}

#[test_case("a\rb\rc",       "a\nb\nc"       ; "bare_cr")]
#[test_case("a\r\nb\r\nc",   "a\nb\nc"       ; "crlf")]
#[test_case("a\r\nb\rc\nd",  "a\nb\nc\nd"    ; "mixed")]
fn paste_normalizes_line_endings(input: &str, expected: &str) {
    let mut app = test_app();
    app.update(Msg::Paste(input.into()));
    assert_eq!(app.input_box.buffer.value(), expected);
}

#[test]
fn paste_file_path_triggers_image_load() {
    let mut app = test_app();
    app.update(Msg::Paste("file:///tmp/nonexistent.png".into()));
    assert!(!app.image_paste_rx.is_empty());
    assert_eq!(app.input_box.buffer.value(), "");
}

#[test]
fn submit_during_streaming_queues_message() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::SendMessage(_)));
    assert_eq!(app.status, Status::Streaming);

    app.update(Msg::Key(key(KeyCode::Char('b'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(actions.is_empty());
    assert_eq!(app.queue.len(), 1);
}

#[test]
fn queue_item_consumed_pushes_deferred_user_message() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    assert_eq!(app.main_chat().message_count(), 1);

    app.queue_and_notify(queued_msg("queued"));
    assert_eq!(
        app.main_chat().message_count(),
        1,
        "queueing while streaming must not render the bubble yet",
    );

    app.update(agent_msg_with_run_id(
        AgentEvent::QueueItemConsumed {
            text: "queued".into(),
            image_count: 0,
        },
        app.run_id,
    ));

    assert_eq!(app.main_chat().message_count(), 2);
    assert_eq!(app.main_chat().last_message_text(), "queued");
    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::User),
    );
}

/// Restored queue items start runs without `start_run`, so the consumed
/// event is the only signal that the agent went busy: it must flip status
/// or the busy-guard and esc-to-cancel stay off during the whole run.
#[test]
fn queue_item_consumed_marks_agent_streaming() {
    let mut app = test_app();
    assert_eq!(app.status, Status::Idle);

    app.update(agent_msg_with_run_id(
        AgentEvent::QueueItemConsumed {
            text: "restored".into(),
            image_count: 0,
        },
        app.run_id,
    ));

    assert_eq!(app.status, Status::Streaming);
}

#[test_case(error_app as fn(&mut App) ; "error")]
#[test_case(cancel_app as fn(&mut App) ; "cancel")]
fn clears_queue(terminate: fn(&mut App)) {
    let mut app = app_with_queued_message();
    terminate(&mut app);
    assert!(app.queue.is_empty());
}

#[test_case("/compact" ; "slash_command")]
#[test_case("exit" ; "exit_keyword")]
#[test_case("!ls" ; "shell_prefix")]
fn submit_prompt_never_interprets_text(text: &str) {
    let mut app = test_app();
    match app.submit_prompt(queued_msg(text)) {
        SubmitOutcome::Started(actions) => {
            assert!(matches!(&actions[0], Action::SendMessage(_)))
        }
        _ => panic!("raw prompt must start the agent"),
    }
}

#[test]
fn submit_prompt_queues_while_streaming() {
    let mut app = test_app();
    app.status = Status::Streaming;
    assert!(matches!(
        app.submit_prompt(queued_msg("hi")),
        SubmitOutcome::Queued
    ));
    assert_eq!(app.queue.len(), 1);
}

#[test_case(test_app as fn() -> App, "   ", queue::EMPTY_PROMPT_ERR ; "blank_text")]
#[test_case(streaming_app_without_queue, "hi", queue::NO_QUEUE_ERR ; "streaming_without_shared_queue")]
fn submit_prompt_rejects(mk: fn() -> App, text: &str, expected: &str) {
    let mut app = mk();
    match app.submit_prompt(queued_msg(text)) {
        SubmitOutcome::Rejected(e) => assert_eq!(e, expected),
        _ => panic!("expected rejection"),
    }
}

fn streaming_app_without_queue() -> App {
    let dir = StateDir::from_path(env::temp_dir());
    let mut app = build_app(dir.clone(), Arc::new(test_writer(dir)));
    app.status = Status::Streaming;
    app
}

fn session_rule() -> PermissionRule {
    PermissionRule {
        tool: ToolKey::parse("bash").unwrap(),
        scope: None,
        effect: Effect::Allow,
    }
}

fn queued_msg(text: &str) -> QueuedMessage {
    QueuedMessage {
        text: text.into(),
        images: vec![],
    }
}

fn app_with_queued_message() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("queued"));
    app
}

fn type_and_submit(app: &mut App, text: &str) -> Vec<Action> {
    for c in text.chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)))
}

pub(crate) fn cancel_app(app: &mut App) {
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
}

pub(crate) fn error_app(app: &mut App) {
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
}

/// Splits the line the way the palette does, so a test can write what the
/// user types.
fn cmd(cmdline: &str) -> ParsedCommand {
    let (name, args) = cmdline
        .split_once(char::is_whitespace)
        .unwrap_or((cmdline, ""));
    ParsedCommand {
        name: name.to_string(),
        args: args.trim().to_string(),
        bang: false,
    }
}

fn type_slash(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('/'))));
}

#[test]
fn typing_filters_palette() {
    let mut app = test_app();
    type_slash(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Char('z'))));
    assert!(!app.command_palette.is_active());
}

#[test]
fn enter_executes_new_command() {
    let mut app = test_app();
    type_slash(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::NewSession));
    assert!(!app.command_palette.is_active());
}

#[test]
fn ctrl_c_closes_palette() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(!app.command_palette.is_active());
}

/// The event exists so plugins can drop what belonged to the session that
/// ended. Naming its replacement makes every such handler a no-op.
#[test]
fn session_reset_names_the_session_that_ended() {
    let mut app = test_app();
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;
    let ended = app.state.session.id.to_string();

    app.reset_session();

    let (event, data) = probe.try_recv_autocmd().expect("SessionReset fired");
    assert_eq!(event, "SessionReset");
    assert_eq!(data["session_id"], serde_json::json!(ended));
    let (ended_id, reason) = probe.try_recv_end_session().expect("SessionEnd queued");
    assert_eq!(ended_id.to_string(), ended);
    assert_eq!(reason, SessionEndReason::Reset);
    assert_ne!(
        app.state.session.id.to_string(),
        ended,
        "reset must have installed a different session, or this proves nothing"
    );
}

#[test]
fn reset_session_clears_plan() {
    let mut app = test_app();
    app.state.token_usage.input = 500;
    app.chats[0].context_size = 1000;
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.queue_and_notify(queued_msg("q"));
    app.queue.set_focus_at(0);
    app.help_modal.toggle();
    let (_tx, rx) = flume::bounded::<crate::components::btw_modal::BtwEvent>(1);
    app.btw_modal.open("q", rx);
    let actions = app.reset_session();
    assert!(matches!(&actions[0], Action::NewSession));
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.state.token_usage.input, 0);
    assert_eq!(app.chats[0].context_size, 0);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan, PlanState::None);
    assert!(app.queue.is_empty());
    assert!(app.recoverable_queue.is_empty());
    assert_eq!(app.chats.len(), 1);
    assert_eq!(app.chats[0].name, "Main");
    assert_eq!(app.active_chat, 0);
    assert!(app.chat_index.is_empty());
    assert!(app.queue.focus().is_none());
    assert!(!app.help_modal.is_open());
    assert!(!app.btw_modal.is_open());
}

/// A new session inheriting the plan path, the draft or the queue of the one it
/// started from would take over work it never did, and the checkpoint here is
/// what puts all of that in the old session's meta. The whole meta is asserted,
/// so a field that starts riding along cannot slip by.
#[test]
fn blank_session_carries_the_settings_that_outlive_a_turn() {
    let mut app = test_app();
    app.state.thinking = ThinkingConfig::Effort(Effort::High);
    app.state.fast = true;
    app.state.workflow = true;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.state.context_size = MEASURED_CONTEXT;
    app.input_box.set_input("half a thought".into());
    app.queue_and_notify(queued_msg("q"));
    app.permissions.load_session_rules(vec![session_rule()]);
    app.permissions.set_session_yolo(Some(true));
    app.checkpoint();

    let session = app.blank_session();

    assert_eq!(
        session.meta,
        SessionMeta {
            mode: Some(StoredMode::Plan),
            thinking: Some(StoredThinking::Effort {
                level: Effort::High
            }),
            fast: true,
            workflow: true,
            yolo: Some(true),
            ..Default::default()
        }
    );
    assert!(session.messages().is_empty());
    assert_eq!(session.model, app.state.model.spec());
    assert_eq!(session.cwd, app.state.session.cwd);
}

/// A setting written into the meta but never read back still opens the tab
/// wrong, so only the round trip through a whole `App` proves `Ctrl-N` works.
/// Yolo rides in the permission manager rather than in `SessionState`, which
/// is how it stayed unchecked while the rest was covered.
#[test]
fn a_spawned_tab_opens_on_the_settings_it_was_started_with() {
    let mut app = test_app();
    set_opus_model(&mut app);
    app.state.thinking = ThinkingConfig::Effort(Effort::High);
    app.state.fast = true;
    app.state.workflow = true;
    app.state.mode = Mode::Plan;
    app.permissions.toggle_yolo();

    let spawned = spawned_app(app.blank_session(), test_permissions(false));

    assert_eq!(spawned.state.thinking, app.state.thinking);
    assert_eq!(spawned.state.fast, app.state.fast);
    assert_eq!(spawned.state.workflow, app.state.workflow);
    assert_eq!(spawned.state.mode, app.state.mode);
    assert!(
        spawned.permissions.is_yolo(),
        "the toggle is the user's, so it opens the next tab too"
    );
    assert!(
        spawned.state.plan.path().is_some(),
        "a plan-mode tab owes itself a plan file"
    );
}

/// `--yolo` seeds the prototype every tab forks from, and `/yolo` off only ever
/// reaches the fork the tab holds. A new tab that trusts its fork reopens
/// auto-approving everything, so the meta `blank_session` just wrote is the one
/// place that answer survives.
#[test]
fn a_spawned_tab_honours_the_yolo_turned_off_under_the_flag() {
    let prototype = test_permissions(true);
    let app = spawned_app(
        AppSession::new(TEST_MODEL_SPEC, TEST_CWD),
        Arc::new(prototype.fork()),
    );
    assert!(app.permissions.is_yolo(), "--yolo seeds the first tab");

    app.permissions.toggle_yolo();
    let session = app.blank_session();
    assert_eq!(session.meta.yolo, Some(false));

    let spawned = spawned_app(session, Arc::new(prototype.fork()));

    assert!(
        !spawned.permissions.is_yolo(),
        "forking the prototype must not bring the flag back"
    );
}

#[test]
fn reset_session_assigns_new_plan_path_in_plan_mode() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("old-plan.md"));
    app.reset_session();
    assert_eq!(app.state.mode, Mode::Plan);
    assert!(app.state.plan.path().is_some());
    assert_ne!(app.state.plan.path(), Some(Path::new("old-plan.md")));
}

#[test]
fn reset_session_clears_drafting_plan_in_build_mode() {
    let mut app = test_app();
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Drafting(PathBuf::from("leftover.md"));
    app.reset_session();
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan, PlanState::None);
}

/// A retried write hits the same transition again, and the plugin still gets
/// one event with the draft path it needs to open.
#[test]
fn plan_ready_fires_once_per_draft() {
    let mut app = test_app();
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;

    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from(PLAN_DRAFT_PATH));
    app.transition_plan(PlanTrigger::WriteDone);
    app.transition_plan(PlanTrigger::WriteDone);

    let (event, data) = probe.try_recv_autocmd().expect("PlanReady fired");
    assert_eq!(event, PLAN_READY_EVENT);
    assert_eq!(data["path"], serde_json::json!(PLAN_DRAFT_PATH));
    assert!(
        probe.try_recv_autocmd().is_none(),
        "second WriteDone must not re-emit"
    );
}

#[test]
fn plan_ready_does_not_fire_outside_plan_mode() {
    let mut app = test_app();
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;

    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Drafting(PathBuf::from(PLAN_DRAFT_PATH));
    app.transition_plan(PlanTrigger::WriteDone);

    assert!(probe.try_recv_autocmd().is_none());
}

#[test]
fn load_session_clears_plan() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    app.state
        .session_mut()
        .push_message(Message::user("test".into()));
    app.state.session_mut().save(&app.storage).unwrap();
    let id = app.state.session.id;
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Ready(PathBuf::from("old-plan.md"));
    app.load_session(id);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan.path(), None);
}

#[test]
fn tool_lifecycle_events_name_the_session_and_tool() {
    let mut app = streaming_app();
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;
    let session_id = app.state.session.id.to_string();

    app.update(agent_msg(tool_start("tool-1", "bash")));

    let (event, data) = probe.try_recv_autocmd().expect("ToolStart fired");
    assert_eq!(event, "ToolStart");
    assert_eq!(data["session_id"], serde_json::json!(session_id));
    assert_eq!(data["tool_id"], "tool-1");
    assert_eq!(data["tool"], "bash");

    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "tool-1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("done".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    }))));

    let (event, data) = probe.try_recv_autocmd().expect("ToolDone fired");
    assert_eq!(event, "ToolDone");
    assert_eq!(data["session_id"], serde_json::json!(session_id));
    assert_eq!(data["tool_id"], "tool-1");
    assert_eq!(data["tool"], "bash");

    app.run_id += 1;
    app.update(agent_msg_with_run_id(tool_start("stale", "read"), 1));
    assert!(probe.try_recv_autocmd().is_none());
}

#[test]
fn tab_in_palette_completes_command() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Tab)));
    let val = app.input_box.buffer.value();
    assert!(val.starts_with('/'));
}

#[test]
fn chat_navigation_actions() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "sub".into() },
        TASK_ID,
        Some("research"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.active_chat, 0);

    app.run_builtin(BuiltinAction::NextChat);
    assert_eq!(app.active_chat, 1);

    app.run_builtin(BuiltinAction::NextChat);
    assert_eq!(app.active_chat, 1);

    app.run_builtin(BuiltinAction::PrevChat);
    assert_eq!(app.active_chat, 0);

    app.run_builtin(BuiltinAction::PrevChat);
    assert_eq!(app.active_chat, 0);
}

#[test]
fn subagents_get_descriptive_names() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "a".into() },
        TASK_ID,
        Some("first"),
    ));
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "b".into() },
        "task2",
        Some("second"),
    ));
    assert_eq!(app.chats.len(), 3);
    assert_eq!(app.chats[1].name, "first");
    assert_eq!(app.chats[2].name, "second");
}

#[test]
fn subagent_prompt_shown_once_and_not_duplicated() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg_with_prompt(
        AgentEvent::TextDelta { text: "a".into() },
        TASK_ID,
        Some("research"),
        Some("Find all TODO comments"),
    ));
    assert_eq!(app.chats[1].message_count(), 1);
    assert_eq!(app.chats[1].last_message_text(), "Find all TODO comments");

    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "b".into() },
        TASK_ID,
        Some("research"),
    ));
    app.chats[1].flush();
    assert_eq!(app.chats[1].message_count(), 2);
    assert_eq!(app.chats[1].last_message_text(), "ab");
}

#[test]
fn turn_complete_tracks_usage_and_context_per_chat() {
    let mut app = app_with_subagent();

    let main_usage = TokenUsage {
        input: 100,
        output: 50,
        ..Default::default()
    };
    app.update(agent_msg(turn_complete(main_usage, "test", None)));

    let sub_usage = TokenUsage {
        input: 200,
        output: 75,
        ..Default::default()
    };
    app.update(subagent_msg(
        turn_complete(sub_usage, "test", None),
        TASK_ID,
        None,
    ));

    assert_eq!(app.state.token_usage.input, 300);
    assert_eq!(app.state.token_usage.output, 125);
    assert_eq!(app.chats[0].context_size, main_usage.context_tokens());
    assert_eq!(app.chats[1].context_size, sub_usage.context_tokens());
}

const SUBAGENT_NAME: &str = "research";
const SUB_TOKENS: TokenUsage = TokenUsage {
    input: 1_000,
    output: 200,
    cache_creation: 300,
    cache_read: 400,
    cost: None,
};
const SUB_COST: Option<f64> = Some(0.007);
const MAIN_TOKENS: TokenUsage = TokenUsage {
    input: 500,
    output: 100,
    cache_creation: 0,
    cache_read: 0,
    cost: None,
};
const MAIN_COST: Option<f64> = Some(0.002);
const MAIN_MODEL: &str = "main-model";

fn main_turn() -> Msg {
    agent_msg(turn_complete(MAIN_TOKENS, MAIN_MODEL, MAIN_COST))
}

fn sub_turn_complete() -> Msg {
    subagent_msg(
        turn_complete(SUB_TOKENS, "child-model", SUB_COST),
        TASK_ID,
        Some(SUBAGENT_NAME),
    )
}

/// Built with the header's own formatter: these tests pin which tool gets the
/// usage, not how it is spelled (maki-providers covers the spelling).
fn sub_usage_text() -> String {
    SUB_TOKENS.format_sum_cost(SUB_COST)
}

/// Each turn bills at the rates of the model that ran it, subagent tiers
/// included, so the session total is the sum of what the turns recorded.
#[test]
fn session_cost_sums_what_each_model_recorded() {
    let mut app = streaming_app();
    app.update(main_turn());
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(sub_turn_complete());

    let expected = MAIN_COST.unwrap() + SUB_COST.unwrap();
    assert_eq!(app.state.cost, Some(expected));
    let stored: f64 = app
        .state
        .session
        .usage_by_model()
        .values()
        .filter_map(|u| u.cost)
        .sum();
    assert_eq!(stored, expected);
}

const RESTORED_COST: f64 = 0.42;
/// Counters big enough that re-pricing them could never land on
/// [`RESTORED_COST`], so a total derived from them stands out.
const RESTORED_TOKENS: TokenUsage = TokenUsage {
    input: 1_000_000,
    output: 0,
    cache_creation: 0,
    cache_read: 0,
    cost: None,
};
const RESTORED_MODEL: &str = "model-that-ran-before";
const SIGMA_MISSING: &str = "the status bar must draw the session total";
const COST_WAS_NOT_BILLED: &str = "the turn must bill something for the reset to prove anything";

/// A new session opens on a clean bill. The total is never re-derived from the
/// counters, so anything left behind here follows the user forever.
#[test]
fn reset_session_clears_the_bill_and_the_model_breakdown() {
    let mut app = streaming_app();
    app.update(main_turn());
    assert_eq!(app.state.cost, MAIN_COST, "{COST_WAS_NOT_BILLED}");

    app.reset_session();

    assert_eq!(app.state.cost, None);
    assert!(app.state.session.usage_by_model().is_empty());
}

/// `None` is what hides the cost, so an unpriced turn must leave the total
/// alone. `Some(0.0)` would advertise a free session.
#[test_case(None, None ; "unpriced_turns_only")]
#[test_case(MAIN_COST, MAIN_COST ; "priced_turn_after_an_unpriced_one")]
fn session_cost_counts_only_priced_turns(second: Option<f64>, expected: Option<f64>) {
    let mut app = streaming_app();
    // How an unpriced session opens; `session_state` covers the seeding.
    app.state.cost = None;
    app.update(agent_msg(turn_complete(MAIN_TOKENS, MAIN_MODEL, None)));

    app.update(agent_msg(turn_complete(MAIN_TOKENS, MAIN_MODEL, second)));

    assert_eq!(app.state.cost, expected);
}

/// The restored bill is a running total later turns add to, so a resumed
/// session shows what it paid back then plus what it pays now, never its
/// counters re-priced at today's rates.
#[test]
fn resumed_session_keeps_adding_to_the_restored_bill() {
    let mut app = test_app();
    let mut stored = AppSession::new("test-model", "/tmp");
    stored.token_usage = RESTORED_TOKENS;
    stored.add_model_usage(RESTORED_MODEL, RESTORED_TOKENS.billed(Some(RESTORED_COST)));

    app.apply_loaded_session(stored, &test_model());
    assert_eq!(app.state.cost, Some(RESTORED_COST));
    assert_eq!(app.chats[0].cost, Some(RESTORED_COST));

    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(main_turn());

    assert_eq!(app.state.cost, Some(RESTORED_COST + MAIN_COST.unwrap()));
}

/// The sigma the status bar draws once subagents split the bill is the session
/// total itself, so it cannot drift from what `/usage` sums.
#[test]
fn status_bar_sigma_draws_the_session_cost() {
    let mut app = app_with_subagent();
    app.update(main_turn());
    app.update(sub_turn_complete());

    let total = app.state.cost.expect("both turns were priced");
    let sigma = format!("\u{03a3}${total:.3}");
    assert!(
        rendered(&mut app).contains(&sigma),
        "{SIGMA_MISSING}: {sigma}"
    );
}

#[test]
fn subagent_turn_complete_updates_matching_parent_header_with_last_turn() {
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(agent_msg(tool_start("task2", "task")));

    app.update(sub_turn_complete());
    // The second turn's tokens differ, so a sum or a stale first turn would fail.
    let last = TokenUsage {
        input: 42,
        ..SUB_TOKENS
    };
    app.update(subagent_msg(
        turn_complete(last, "child-model", SUB_COST),
        TASK_ID,
        Some(SUBAGENT_NAME),
    ));

    let expected = last.format_sum_cost(SUB_COST.map(|cost| cost * 2.0));
    assert_eq!(
        app.chats[0].tool_turn_usage(TASK_ID),
        Some(expected.as_str())
    );
    assert_eq!(app.chats[0].tool_turn_usage("task2"), None);
}

#[test_case(false ; "plain_tool_takes_the_parent_turn")]
#[test_case(true  ; "subagent_stamp_is_not_overwritten")]
fn parent_turn_flush_stamps_the_last_unstamped_tool(subagent_ran: bool) {
    let mut app = streaming_app();
    app.update(main_turn());
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    if subagent_ran {
        app.update(sub_turn_complete());
    }

    app.update(agent_msg(tool_results_submitted()));

    let expected = if subagent_ran {
        sub_usage_text()
    } else {
        MAIN_TOKENS.format(MAIN_COST)
    };
    assert_eq!(
        app.chats[0].tool_turn_usage(TASK_ID),
        Some(expected.as_str())
    );
}

#[test]
fn tool_inside_subagent_chat_gets_its_turn_usage() {
    const TOOL_ID: &str = "sub_bash";
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(subagent_msg(
        tool_start(TOOL_ID, "bash"),
        TASK_ID,
        Some(SUBAGENT_NAME),
    ));
    app.update(sub_turn_complete());

    app.update(subagent_msg(
        tool_results_submitted(),
        TASK_ID,
        Some(SUBAGENT_NAME),
    ));

    assert_eq!(
        app.chats[1].tool_turn_usage(TOOL_ID),
        Some(SUB_TOKENS.format(SUB_COST).as_str())
    );
}

#[test]
fn turn_complete_accumulates_usage_by_model() {
    let mut app = app_with_subagent();

    app.update(agent_msg(turn_complete(
        TokenUsage {
            input: 100,
            output: 50,
            cache_read: 10,
            ..Default::default()
        },
        "main-model",
        None,
    )));
    app.update(subagent_msg(
        turn_complete(
            TokenUsage {
                input: 200,
                output: 75,
                ..Default::default()
            },
            "sub-model",
            None,
        ),
        TASK_ID,
        None,
    ));

    let by_model = app.state.session.usage_by_model();
    assert_eq!(by_model.len(), 2);
    let main = &by_model["main-model"];
    assert_eq!(main.input, 100);
    assert_eq!(main.output, 50);
    assert_eq!(main.cache_read, 10);
    let sub = &by_model["sub-model"];
    assert_eq!(sub.input, 200);
    assert_eq!(sub.output, 75);
}

#[test]
fn cancel_resets_all_chats_and_indices() {
    let mut app = app_with_subagent();
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "sub_t1".into(),
            tool: "bash".into(),
            summary: "running".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        TASK_ID,
        None,
    ));
    let buf = Arc::new(maki_agent::SharedBuf::new());
    app.update(subagent_msg(
        AgentEvent::LiveToolBuf {
            id: "sub_t1".into(),
            body: buf,
        },
        "task1",
        None,
    ));

    let actions = app.handle_cancel();
    assert!(matches!(actions.as_slice(), [Action::CancelAgent { .. }]));
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chats[1].is_finished());
    assert!(app.chat_index.is_empty());
    assert_eq!(app.cadence(), Cadence::IDLE);
}

/// What a subagent's own session sends when it closes, which the `task` tool
/// does before it reports success or failure.
pub(crate) fn close_subagent_transcript(app: &mut App, id: &str) {
    app.update(agent_msg(AgentEvent::SubagentHistory {
        tool_use_id: id.into(),
        messages: vec![],
    }));
}

pub(crate) fn finish_subagent(app: &mut App, id: &str, is_error: bool) {
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: id.into(),
        tool: "task".into(),
        output: ToolOutput::Plain("result".into()),
        is_error,
        annotation: None,
        written_path: None,
    }))));
}

fn finish_subagent_task(app: &mut App, is_error: bool) {
    finish_subagent(app, TASK_ID, is_error);
}

#[test]
fn subagent_done_only_in_subagent_chat() {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    assert_ne!(app.chats[0].last_message_role(), Some(&DisplayRole::Done));
}

#[test_case(|app: &mut App| finish_subagent_task(app, false), DONE_TEXT,      &DisplayRole::Done  ; "task_success")]
#[test_case(|app: &mut App| finish_subagent_task(app, true),  ERROR_TEXT,     &DisplayRole::Error ; "task_failure")]
#[test_case(cancel_app as fn(&mut App),                       CANCELLED_TEXT, &DisplayRole::Error ; "cancel")]
#[test_case(error_app  as fn(&mut App),                       ERROR_TEXT,     &DisplayRole::Error ; "main_error")]
fn subagent_terminal_marker(
    terminate: fn(&mut App),
    expected_text: &str,
    expected_role: &DisplayRole,
) {
    let mut app = app_with_subagent();
    terminate(&mut app);
    assert_eq!(app.chats[1].last_message_text(), expected_text);
    assert_eq!(app.chats[1].last_message_role(), Some(expected_role));
}

#[test_case(error_app  as fn(&mut App) ; "error")]
#[test_case(cancel_app as fn(&mut App) ; "cancel")]
fn subagent_already_done_not_double_marked(terminate: fn(&mut App)) {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    let count_before = app.chats[1].message_count();
    terminate(&mut app);
    assert_eq!(app.chats[1].message_count(), count_before);
    assert_eq!(app.chats[1].last_message_text(), DONE_TEXT);
}

#[test_case(false, DONE_TEXT,  &DisplayRole::Done  ; "batch_subagent_success")]
#[test_case(true,  ERROR_TEXT, &DisplayRole::Error ; "batch_subagent_failure")]
fn batch_subagent_done_marker(is_error: bool, expected_text: &str, expected_role: &DisplayRole) {
    let mut app = app_with_subagent_id("batch1__0");
    finish_subagent(&mut app, "batch1__0", is_error);
    assert_eq!(app.chats[1].last_message_text(), expected_text);
    assert_eq!(app.chats[1].last_message_role(), Some(expected_role));
}

fn streaming_app() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app
}

pub(crate) fn start_subagent(app: &mut App, id: &str, name: &str) {
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "x".into() },
        id,
        Some(name),
    ));
}

pub(crate) fn app_with_subagent_id(id: &str) -> App {
    let mut app = streaming_app();
    start_subagent(&mut app, id, RESEARCH_NAME);
    app
}

fn app_with_subagent() -> App {
    app_with_subagent_id(TASK_ID)
}

/// The shape the picker filters on: the main chat first without a status, then
/// every status a subagent can report, spelled the way Lua reads it.
#[test]
fn tasks_report_main_chat_then_subagent_outcomes() {
    let mut app = app_with_subagent_id("task1");
    for (id, name) in [("task2", "build"), ("task3", "deploy")] {
        app.update(subagent_msg(
            AgentEvent::TextDelta { text: "y".into() },
            id,
            Some(name),
        ));
    }
    finish_subagent(&mut app, "task1", false);
    finish_subagent(&mut app, "task2", true);

    let tasks = serde_json::to_value(app.tasks()).unwrap();
    assert_eq!(
        tasks,
        serde_json::json!([
            { "id": "main", "name": "Main", "focused": true },
            { "id": "task1", "name": "research", "status": "done", "focused": false },
            { "id": "task2", "name": "build", "status": "error", "focused": false },
            { "id": "task3", "name": "deploy", "status": "working", "focused": false },
        ])
    );
}

/// Escaping out of a subagent takes the single-chat cancel path instead of the
/// sweep over the whole turn, and that path has to land the task in `error`
/// too, or it spins forever.
#[test]
fn cancelling_from_inside_a_subagent_reports_error() {
    let mut app = app_with_subagent();
    app.focus_task(TASK_ID).unwrap();
    cancel_app(&mut app);
    assert_eq!(
        serde_json::to_value(app.tasks()).unwrap()[1]["status"],
        serde_json::json!("error")
    );
}

const OVERLAY_BLOCKED_KEYS: &[KeyEvent] = &[
    kb::SCROLL_HALF_UP.to_key_event(),
    kb::SCROLL_HALF_DOWN.to_key_event(),
    kb::HELP.to_key_event(),
];

fn open_help(app: &mut App) {
    app.help_modal.toggle();
}

fn open_search(app: &mut App) {
    app.search_modal.open(ScrollPos::default(), true);
}

fn focus_queue(app: &mut App) {
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("q"));
    app.queue.set_focus_at(0);
}

#[test_case(open_help as fn(&mut App) ; "help_modal")]
#[test_case(open_search               ; "search_modal")]
#[test_case(focus_queue               ; "queue_focus")]
fn overlay_blocks_ctrl_shortcuts(setup: fn(&mut App)) {
    let mut app = app_with_subagent();
    setup(&mut app);
    let before = app.active_chat;
    let scroll_before = app.chats[app.active_chat].scroll_pos();

    for k in OVERLAY_BLOCKED_KEYS {
        app.update(Msg::Key(*k));
    }

    assert_eq!(
        app.active_chat, before,
        "active_chat changed through overlay"
    );
    assert_eq!(
        app.chats[app.active_chat].scroll_pos(),
        scroll_before,
        "scroll changed through overlay"
    );
}

const COMPACT_GUIDANCE: &str = "keep the failing test names";
const COMPACT_WITH_GUIDANCE: &str = "/compact keep the failing test names";

#[test_case("/compact", None ; "no_guidance")]
#[test_case(COMPACT_WITH_GUIDANCE, Some(COMPACT_GUIDANCE) ; "guidance_forwarded")]
fn compact_command_sets_streaming(cmdline: &str, expected: Option<&str>) {
    let mut app = test_app();
    let actions = app.execute_command(cmd(cmdline), 0);
    assert!(
        matches!(&actions[0], Action::Compact(instructions) if instructions.as_deref() == expected)
    );
    assert_eq!(app.status, Status::Streaming);
}

#[test_case("/compact" ; "bare")]
#[test_case(COMPACT_WITH_GUIDANCE ; "guidance_shown_in_panel")]
fn compact_during_streaming_queues_item(cmdline: &str) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.execute_command(cmd(cmdline), 0);
    assert!(actions.is_empty());
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, cmdline);
}

#[test]
fn cancel_clears_pending_input() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.pending_input = PendingInput::AuthRetry { subagent_id: None };
    cancel_app(&mut app);
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test]
fn scroll_disables_auto_scroll() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 10,
        delta: 3,
    });
    assert!(!app.chats[0].auto_scroll());
}

#[test]
fn scroll_outside_msg_area_ignored() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 25,
        delta: 3,
    });
    assert!(app.chats[0].auto_scroll());
}

#[test]
fn scroll_shortcuts_toggle_auto_scroll() {
    let mut app = test_app();
    app.active_chat().enable_auto_scroll();
    app.update(Msg::Key(kb::SCROLL_TOP.to_key_event()));
    assert!(!app.chats[0].auto_scroll());
    app.update(Msg::Key(kb::SCROLL_BOTTOM.to_key_event()));
    assert!(app.chats[0].auto_scroll());
}

/// A selection names a place in the transcript, so the transcript has to
/// exist before a drag can land anywhere.
fn app_with_transcript(zone: Rect) -> App {
    let mut app = test_app();
    for i in 0..50 {
        app.active_chat()
            .push(DisplayMessage::new(DisplayRole::User, format!("line {i}")));
    }
    set_zone(&mut app, SelectionZone::Messages, zone);
    let backend = ratatui::backend::TestBackend::new(zone.width, zone.bottom());
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| app.active_chat().view(frame, zone, false))
        .unwrap();
    app
}

fn drag_end(app: &App) -> (RowPos, u16) {
    let (_, end) = app.selection_state.as_ref().unwrap().sel().normalized();
    (app.chats[app.active_chat].project_row(end), end.col)
}

#[test_case((20, 10), RowPos::At(10), 20, false ; "inside_the_area")]
#[test_case((100, 50), RowPos::At(19), 79, true ; "clamped_to_the_bottom_right")]
fn mouse_drag_follows_the_pointer(drag: (u16, u16), row: RowPos, col: u16, edge_scrolling: bool) {
    let mut app = app_with_transcript(Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        drag.0,
        drag.1,
    ));

    assert_eq!(drag_end(&app), (row, col));
    assert_eq!(
        app.selection_state.as_ref().unwrap().is_edge_scrolling(),
        edge_scrolling,
        "only a drag outside the area edge scrolls"
    );
}

#[test_case(Rect::new(0, 2, 80, 20), (10, 12), (10, 1),  Some(EDGE_SCROLL_LINES)  ; "top_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (10, 22), Some(-EDGE_SCROLL_LINES) ; "bottom_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (20, 15), None                     ; "middle_no_scroll")]
fn edge_scroll_direction(zone: Rect, down: (u16, u16), drag: (u16, u16), expected: Option<i32>) {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        down.0,
        down.1,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        drag.0,
        drag.1,
    ));

    let state = app.selection_state.as_ref().unwrap();
    let edge_dir = match state {
        SelectionState::Dragging { edge_scroll, .. } => edge_scroll.as_ref().map(|es| es.dir),
        _ => None,
    };
    assert_eq!(edge_dir, expected);
}

#[test]
fn mouse_up_clears_edge_scroll() {
    let mut app = app_with_transcript(Rect::new(0, 2, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert!(app.selection_state.as_ref().unwrap().is_edge_scrolling());

    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 1));
    let state = app.selection_state.as_ref().unwrap();
    assert!(state.is_pending_copy());
}

#[test]
fn double_esc_cancels_flushes_and_fails_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta {
        text: "partial".into(),
    }));
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "t1".into(),
        tool: "bash".into(),
        summary: "running".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());

    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.chats[0].in_progress_count(), 0);
}

#[test]
fn double_esc_idle_opens_rewind_picker() {
    let mut app = test_app();
    type_and_submit(&mut app, "hello");
    app.status = Status::Idle;
    app.run_id = 1;
    app.state
        .session_mut()
        .push_message(Message::user("hello".into()));

    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.rewind_picker.is_open());
}

#[test]
fn double_esc_idle_no_user_turns_flashes_error() {
    let mut app = test_app();
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.rewind_picker.is_open());
}

#[test]
fn ctrl_c_while_streaming_cancels_instead_of_quitting() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert_eq!(app.status, Status::Idle);
    assert_ne!(app.exit_request, ExitRequest::Success);
}

/// The whole point of issue 778: a settled session paints nothing at all. Any
/// poller that starts reporting a change on every tick trips this.
#[test]
fn settled_app_owes_no_frame_and_does_not_animate() {
    let mut app = app_without_splash();

    assert_eq!(app.cadence(), Cadence::IDLE);
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
}

/// Nothing wakes the loop when a background thread drops an answer into a
/// shared slot, so `tick` has to go and look. The tick that first sees it is
/// also the only one allowed to claim a frame: `tick` runs on every turn of the
/// loop, so a poller that keeps saying yes never lets it sleep again.
#[track_caller]
fn assert_owes_one_frame(app: &mut App, arrival: impl FnOnce()) {
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
    arrival();
    assert_eq!(app.tick(), Dirty::YES, "{OWED}");
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
}

/// `/usage` spawns a detached fetch that stores its answer with nothing
/// listening, so an unpolled modal sits on `Loading` until the user presses
/// some unrelated key.
#[test]
fn usage_quota_arriving_in_the_background_owes_a_frame() {
    let mut app = test_app();
    app.execute_command(cmd("/usage"), 0);
    let slot = Arc::clone(&app.usage_slot);

    assert_owes_one_frame(&mut app, || {
        slot.store(Some(Arc::new(UsageFetchState::Loading)));
    });
}

/// Providers publish their model list into a shared slot that wakes nothing,
/// so an open picker keeps showing the stale list until the user happens to
/// press a key.
#[test]
fn model_list_arriving_in_the_background_owes_a_frame() {
    let (mut app, models) = app_with_model_slot();
    app.execute_command(cmd("/model"), 0);
    assert!(app.model_picker.is_open());

    assert_owes_one_frame(&mut app, || {
        models.store(Some(Arc::new(vec![LATE_MODEL_SPEC.into()])));
    });
}

/// Tool output streams into a subagent's chat while the parent chat is the one
/// on screen. Draining only the active chat would lose it, and the task picker
/// and a later switch would show nothing.
#[test]
fn tick_drains_live_bufs_of_background_chats() {
    let mut app = test_app();
    app.run_id = 1;
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(subagent_msg(tool_start(SUB_TOOL_ID, "bash"), TASK_ID, None));
    let buf = Arc::new(maki_agent::SharedBuf::new());
    app.update(subagent_msg(
        AgentEvent::LiveToolBuf {
            id: SUB_TOOL_ID.into(),
            body: Arc::clone(&buf),
        },
        TASK_ID,
        None,
    ));
    assert_eq!(app.active_chat, 0, "the subagent's chat is the hidden one");

    assert_owes_one_frame(&mut app, || {
        buf.append(maki_agent::SnapshotLine::plain(TOOL_OUTPUT_LINE.into()));
    });
}

/// A plugin publishes hints from the Lua thread, and the loop never hears back
/// from that thread. The footer they draw in is on screen the whole time, so a
/// publish nobody polled for shows up on some later, unrelated keypress, or
/// never.
#[test]
fn status_hints_published_by_a_plugin_reach_the_screen() {
    let (mut app, plugin) = app_with_hints();
    plugin.publish(vec![(
        Arc::from(HINT_PLUGIN),
        vec![(HINT_TEXT.into(), HINT_STYLE.into())],
    )]);

    assert!(
        !rendered(&mut app).contains(HINT_TEXT),
        "a hint no poller has seen must not be on screen"
    );
    assert_eq!(app.tick(), Dirty::YES, "{OWED}");
    assert!(rendered(&mut app).contains(HINT_TEXT));

    plugin.publish(vec![]);
    assert_eq!(app.tick(), Dirty::YES, "{OWED}");
    assert!(!rendered(&mut app).contains(HINT_TEXT));
}

fn rendered(app: &mut App) -> String {
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            app.view(frame);
        })
        .unwrap();
    buffer_text(terminal.backend().buffer())
}

/// The event loop parks the terminal cursor on whatever `view` reports, so an
/// IME anchors its preedit text there. The report has to be the very cell the
/// input box reversed for its software cursor, and the hardware cursor has to
/// stay hidden: shown, it would invert that cell back to plain text.
#[test]
fn view_reports_the_reversed_input_cell_and_hides_the_hardware_cursor() {
    let mut app = test_app();
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut draw = |app: &mut App| {
        let mut cursor = None;
        terminal.draw(|frame| cursor = app.view(frame)).unwrap();
        assert!(
            !terminal.backend().cursor_visible(),
            "{CURSOR_STAYS_HIDDEN}"
        );
        cursor.map(|pos| {
            let cell = terminal
                .backend()
                .buffer()
                .cell(pos)
                .expect(CURSOR_ON_SCREEN);
            (pos, cell.modifier.contains(Modifier::REVERSED))
        })
    };

    assert!(
        matches!(draw(&mut app), Some((_, true))),
        "{CURSOR_ON_REVERSED_CELL}"
    );

    app.update(Msg::Key(kb::HELP.to_key_event()));
    assert_eq!(draw(&mut app), None, "{OVERLAY_TAKES_THE_CURSOR}");
}

/// When the picker gives up on a directory it cannot list, the flash is the
/// only trace the user gets. Forwarding it moved from `view` into `tick`, and
/// dropping that hop closes the picker with no explanation at all. The loop
/// ends the moment the walker thread answers; the deadline only turns a
/// missing hop into a failure instead of a hang.
#[test]
fn tick_forwards_the_file_picker_flash_to_the_status_bar() {
    let tmp = TempDir::new().unwrap();
    let mut app = test_app();
    app.file_picker
        .open(&tmp.path().join(MISSING_DIR).to_string_lossy());

    let deadline = Instant::now() + WALK_TIMEOUT;
    while app.status_bar.flash_text().is_none() {
        assert!(Instant::now() < deadline, "the picker never flashed");
        let _ = app.tick();
        std::thread::yield_now();
    }

    assert_eq!(app.status_bar.flash_text(), Some(UNREADABLE_DIR_MSG));
    assert!(!app.file_picker.is_open());
}

/// A waiting tool draws a spinner, which changes once per `SPINNER_FRAME`.
/// Claiming `SMOOTH` here paints five identical frames for every visible one,
/// for as long as the tool runs.
#[test]
fn waiting_tool_animates_at_the_spinner_rate() {
    let mut app = app_without_splash();
    app.update(agent_msg(tool_start("t1", "bash")));

    assert_eq!(app.cadence(), Cadence::SPINNER);
}

/// The bar spins for a whole streaming turn, again while a restore is in
/// flight, and once more for a retry countdown. The old `is_animating` only
/// knew about the restore, so the other two froze mid turn.
#[test_case(Status::Streaming, false, false => Cadence::SPINNER ; "streaming_turn")]
#[test_case(Status::Idle, true, false => Cadence::SPINNER ; "restoring_session")]
#[test_case(Status::Idle, false, true => Cadence::SPINNER ; "retry_countdown")]
#[test_case(Status::Idle, false, false => Cadence::IDLE ; "nothing_in_flight")]
fn status_bar_motion_reaches_app_cadence(
    status: Status,
    restoring: bool,
    retrying: bool,
) -> Cadence {
    let mut app = app_without_splash();
    app.status = status;
    app.restoring.store(restoring, Ordering::Relaxed);
    if retrying {
        app.retry_info = Some(RetryInfo {
            attempt: 1,
            message: RETRY_MESSAGE.into(),
            deadline: Instant::now() + RETRY_DELAY,
        });
    }
    app.cadence()
}

/// `App::cadence` asks `overlays()` as a group, so a moving overlay only
/// reaches the loop through that fold.
#[test]
fn open_overlay_motion_reaches_app_cadence() {
    let mut app = app_without_splash();
    assert_eq!(app.cadence(), Cadence::IDLE);

    let (event_tx, _event_rx) = flume::bounded::<maki_lua::WinEvent>(8);
    let (_cmd_tx, cmd_rx) = flume::bounded::<maki_lua::WinCommand>(8);
    app.float_mgr.open(
        Arc::new(maki_agent::SharedBuf::new()),
        maki_lua::FloatConfig::default(),
        true,
        event_tx,
        cmd_rx,
    );
    assert_eq!(
        app.cadence(),
        Cadence::SPINNER,
        "an open float's spinners only turn if the app keeps painting"
    );

    app.close_all_overlays();
    assert_eq!(app.cadence(), Cadence::IDLE);
}

#[test]
fn edge_scroll_makes_app_animating() {
    let mut app = app_without_splash();
    assert_eq!(app.cadence(), Cadence::IDLE);
    let zone = Rect::new(0, 2, 80, 20);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert_eq!(
        app.cadence(),
        Cadence::SMOOTH,
        "an edge-scrolling drag advances on a timer, with no events to wake us"
    );
}

#[test]
fn empty_click_clears_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 5));
    assert!(app.selection_state.is_none());
}

fn make_pending_copy(app: &mut App) {
    set_zone(app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 10));
}

const DRAG_ROW: u16 = 5;
const DRAG_COL: u16 = 5;
const SCROLL_LINES: i32 = 3;
const DRAG_ZONE_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 10,
};
const OTHER_ZONE_AREA: Rect = Rect {
    x: 0,
    y: DRAG_ZONE_AREA.height,
    width: 80,
    height: 10,
};

fn send_key(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('a'))));
}

fn send_scroll_outside_drag_zone(app: &mut App) {
    app.update(Msg::Scroll {
        column: DRAG_COL,
        row: OTHER_ZONE_AREA.y + 1,
        delta: SCROLL_LINES,
    });
}

#[test_case(send_key as fn(&mut App) ; "key")]
#[test_case(send_scroll_outside_drag_zone as fn(&mut App) ; "scroll_outside_drag_zone")]
fn interrupt_clears_dragging_but_preserves_pending_copy(interrupt: fn(&mut App)) {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, DRAG_ZONE_AREA);
    set_zone(&mut app, SelectionZone::Input, OTHER_ZONE_AREA);
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        DRAG_COL,
        DRAG_ROW,
    ));
    interrupt(&mut app);
    assert!(app.selection_state.is_none(), "clears dragging");

    make_pending_copy(&mut app);
    interrupt(&mut app);
    assert!(
        app.selection_state.as_ref().unwrap().is_pending_copy(),
        "preserves pending copy"
    );
}

#[test]
fn scroll_preserves_dragging_and_updates_cursor() {
    let area = Rect::new(0, 0, 80, 20);
    let mut app = app_with_transcript(area);

    let bottom = app.active_chat().scroll_pos();
    assert!(
        bottom > ScrollPos::default(),
        "the transcript must overflow the viewport for this to prove anything"
    );

    app.update(Msg::Scroll {
        column: DRAG_COL,
        row: DRAG_ROW,
        delta: SCROLL_LINES,
    });
    let scroll_before = app.active_chat().scroll_pos();
    assert!(
        scroll_before < bottom,
        "scroll up should move the viewport away from the bottom"
    );
    let anchor_before = app.doc_pos(SelectionZone::Messages, area, DRAG_ROW, DRAG_COL);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        DRAG_COL,
        DRAG_ROW,
    ));

    app.update(Msg::Scroll {
        column: DRAG_COL,
        row: DRAG_ROW,
        delta: -SCROLL_LINES,
    });

    assert!(
        matches!(
            app.selection_state.as_ref().unwrap(),
            SelectionState::Dragging { .. }
        ),
        "scroll keeps dragging"
    );

    let (start, end) = app.selection_state.as_ref().unwrap().sel().normalized();
    assert_eq!(start, anchor_before, "anchor keeps its document position");
    let chat = &app.chats[app.active_chat];
    assert_eq!(
        chat.project_row(start),
        RowPos::At(DRAG_ROW - SCROLL_LINES as u16),
        "the anchor rides up by the scrolled lines"
    );
    assert_eq!(
        chat.project_row(end),
        RowPos::At(DRAG_ROW),
        "the cursor stays under the pointer"
    );
    assert_eq!(start.col, DRAG_COL, "anchor column is unchanged");
    assert_eq!(end.col, DRAG_COL, "cursor column is unchanged");
}

#[test]
fn new_mouse_down_replaces_pending_copy_with_dragging() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 15, 15));
    assert!(matches!(
        app.selection_state.as_ref().unwrap(),
        SelectionState::Dragging { .. }
    ));
}

#[test]
fn pending_copy_ignores_drag_and_tick() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 50, 50));
    assert!(app.selection_state.as_ref().unwrap().is_pending_copy());

    let _ = app.tick_edge_scroll();
    assert!(app.selection_state.as_ref().unwrap().is_pending_copy());
}

#[test]
fn pending_copy_not_animating() {
    let mut app = app_without_splash();
    make_pending_copy(&mut app);
    assert_eq!(app.cadence(), Cadence::IDLE);
}

#[test]
fn edge_scroll_direction_switches_on_drag_reversal() {
    let mut app = test_app();
    let zone = Rect::new(0, 5, 80, 10);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 8));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 4));

    if let Some(SelectionState::Dragging { edge_scroll, .. }) = &app.selection_state {
        assert!(
            edge_scroll.as_ref().unwrap().dir > 0,
            "scrolling up (positive dir)"
        );
    } else {
        panic!("expected Dragging");
    }

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 16));
    if let Some(SelectionState::Dragging { edge_scroll, .. }) = &app.selection_state {
        assert!(
            edge_scroll.as_ref().unwrap().dir < 0,
            "scrolling down after reversal"
        );
    } else {
        panic!("expected Dragging");
    }
}

#[test]
fn drag_back_into_area_clears_edge_scroll() {
    let mut app = test_app();
    let zone = Rect::new(0, 5, 80, 10);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 8));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 4));
    assert!(app.selection_state.as_ref().unwrap().is_edge_scrolling());

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    assert!(
        !app.selection_state.as_ref().unwrap().is_edge_scrolling(),
        "dragging back into area must stop edge scroll"
    );
}

#[test]
fn mouse_down_outside_all_zones_ignored() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 40, 10));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 50, 15));
    assert!(
        app.selection_state.is_none(),
        "click outside zones must not create selection"
    );
}

#[test_case(true  ; "non_empty")]
#[test_case(false ; "empty")]
fn queue_command_sets_focus(has_queue: bool) {
    let mut app = if has_queue {
        app_with_queued_message()
    } else {
        test_app()
    };
    app.execute_command(cmd("/queue"), 0);
    assert_eq!(app.queue.focus().is_some(), has_queue);
}

#[test]
fn queue_boundary_clamps() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(0);
    app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.queue.focus(), Some(0), "up at top clamps");
    app.queue.set_focus_at(1);
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.queue.focus(), Some(1), "down at bottom clamps");
}

#[test]
fn queue_enter_removes_selected() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "second");
    assert_eq!(app.queue.focus(), Some(0));
}

#[test]
fn queue_enter_deletes_last_unfocuses() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.queue.is_empty());
    assert!(app.queue.focus().is_none());
}

#[test]
fn queue_esc_unfocuses_without_removing() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.queue.focus().is_none());
    assert_eq!(app.queue.len(), 1);
}

#[test]
fn ctrl_q_pops_front() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "second");
    assert!(app.queue.focus().is_none(), "unfocused stays unfocused");

    app.queue_and_notify(queued_msg("third"));
    app.queue.set_focus_at(1);
    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(
        app.queue.focus(),
        Some(0),
        "focus adjusted when item removed"
    );
}

#[test_case(cancel_app as fn(&mut App) ; "cancel")]
#[test_case(error_app as fn(&mut App)  ; "error")]
fn clears_queue_focus_on_terminate(terminate: fn(&mut App)) {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);
    terminate(&mut app);
    assert!(app.queue.focus().is_none());
}

#[test]
fn stale_events_ignored_after_run_id_increment() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    cancel_app(&mut app);
    let current_run = app.run_id;
    let actions = type_and_submit(&mut app, "new prompt");
    assert!(matches!(&actions[0], Action::SendMessage(i) if i.message == "new prompt"));
    let active_run = app.run_id;

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "stale text".into(),
        },
        current_run,
    ));
    assert_eq!(app.chats[0].last_message_text(), "new prompt");

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "new text".into(),
        },
        active_run,
    ));
    app.chats[0].flush();
    assert_eq!(app.chats[0].last_message_text(), "new text");
}

#[test]
fn stale_done_does_not_drain_queue() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    cancel_app(&mut app);
    app.queue_and_notify(queued_msg("next"));

    app.update(agent_msg_with_run_id(done(), 1));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.status, Status::Idle);
}

#[test]
fn mouse_down_in_input_creates_input_zone_selection() {
    let mut app = test_app();
    let input = Rect::new(0, 15, 80, 5);
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 15));
    set_zone(&mut app, SelectionZone::Input, input);

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 16));
    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.sel().zone, SelectionZone::Input);
    assert_eq!(state.sel().area, input);
}

#[test]
fn resolve_or_create_chat_sets_model_id_and_annotation() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: TASK_ID.into(),
        tool: "task".into(),
        summary: "research".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));

    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta { text: "hi".into() },
        TASK_ID,
        "research",
        "anthropic/claude-sonnet-4-20250514",
    ));

    assert_eq!(app.chats.len(), 2);
    assert_eq!(
        app.chats[1].model_id.as_deref(),
        Some("anthropic/claude-sonnet-4-20250514")
    );
}

#[test]
fn help_toggles_modal() {
    let mut app = test_app();
    assert!(!app.help_modal.is_open());
    app.update(Msg::Key(kb::HELP.to_key_event()));
    assert!(app.help_modal.is_open());
    app.execute_command(cmd("/help"), 0);
    assert!(!app.help_modal.is_open());
}

#[test]
fn help_modal_consumes_keys_and_esc_closes() {
    let mut app = test_app();
    app.update(Msg::Key(kb::HELP.to_key_event()));

    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert_eq!(app.input_box.buffer.value(), "");

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.help_modal.is_open());
}

#[test_case(
    |_: &mut App| {},
    &[KeybindContext::General, KeybindContext::Editing],
    &[KeybindContext::Streaming]
    ; "idle"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; },
    &[KeybindContext::General, KeybindContext::Streaming, KeybindContext::Editing],
    &[]
    ; "streaming"
)]
#[test_case(
    |app: &mut App| { app.state.mode = Mode::Plan; app.plan_form.on_plan_ready(); },
    &[KeybindContext::FormInput],
    &[KeybindContext::Editing]
    ; "plan_form"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; app.run_id = 1; app.queue_and_notify(queued_msg("q")); app.queue.set_focus_at(0); },
    &[KeybindContext::QueueFocus],
    &[KeybindContext::Editing]
    ; "queue_focus"
)]
#[test_case(
    |app: &mut App| {
        app.state.session_mut().push_message(Message::user("test".into()));
        app.open_rewind_picker();
    },
    &[KeybindContext::RewindPicker],
    &[KeybindContext::Editing]
    ; "rewind_picker"
)]
fn active_contexts(setup: fn(&mut App), expected: &[KeybindContext], absent: &[KeybindContext]) {
    let mut app = test_app();
    setup(&mut app);
    let contexts = app.active_keybind_contexts();
    for ctx in expected {
        assert!(contexts.contains(ctx), "{ctx:?} should be present");
    }
    for ctx in absent {
        assert!(!contexts.contains(ctx), "{ctx:?} should be absent");
    }
}

#[test]
fn submit_exit_quits() {
    let mut app = test_app();
    let actions = app.handle_submit(Submission {
        text: "exit".into(),
        images: vec![],
    });
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(matches!(actions.as_slice(), [Action::ManualExit]));
}

#[test]
fn session_has_content_covers_each_branch() {
    let mut session = AppSession::new("test-model", "/tmp/test");
    assert!(!session_has_content(&session));

    session.meta.input_draft = Some("draft".into());
    assert!(session_has_content(&session));
    session.meta.input_draft = None;

    session.meta.queued_messages = vec!["queued".into()];
    assert!(session_has_content(&session));
    session.meta.queued_messages.clear();

    session.meta.mode = Some(StoredMode::Plan);
    assert!(session_has_content(&session));
    session.meta.mode = Some(StoredMode::Build);

    session.push_message(Message::user("hello".into()));
    assert!(session_has_content(&session));
}

#[test]
fn checkpoint_syncs_ephemeral_content_into_meta() {
    let mut app = test_app();
    app.checkpoint();
    assert!(!session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Char('x'))));
    app.checkpoint();
    assert!(session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Backspace)));
    app.checkpoint();
    assert!(app.state.session.meta.input_draft.is_none());
    assert!(!session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Tab)));
    app.checkpoint();
    assert_eq!(app.state.session.meta.mode, Some(StoredMode::Plan));
    assert!(session_has_content(&app.state.session));

    let mut queued = app_with_queued_message();
    queued.checkpoint();
    let session = &queued.state.session;
    assert!(session.messages().is_empty());
    assert!(session.meta.input_draft.is_none());
    assert_eq!(session.meta.mode, Some(StoredMode::Build));
    assert_eq!(session.meta.queued_messages, vec!["queued".to_string()]);
    assert!(session_has_content(session));
}

#[test]
fn checkpoint_persists_observations_without_using_them_as_title() {
    let mut app = test_app();
    let initial_title = app.state.session.title.clone();
    let _history = attach_live_history(
        &mut app,
        vec![
            Message::observation("build failed".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I will fix it".into(),
                }],
                ..Default::default()
            },
        ],
    );

    app.checkpoint();

    assert_eq!(app.state.session.messages().len(), 2);
    assert!(app.state.session.messages()[0].is_observation());
    assert_eq!(app.state.session.title, initial_title);
}

fn drain_writer(app: App, writer: Arc<StorageWriter>) {
    drop(app);
    Arc::try_unwrap(writer)
        .ok()
        .expect("app must hold the only other writer reference")
        .shutdown(WRITER_DRAIN_TIMEOUT);
}

#[test]
fn reload_persists_session_with_content_to_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.state
        .session_mut()
        .push_message(Message::user("hello".into()));
    let actions = app.execute_command(cmd("/reload"), 0);
    assert_eq!(app.exit_request, ExitRequest::Reload);
    assert!(matches!(actions.as_slice(), [Action::ManualExit]));
    app.checkpoint();
    let id = app.state.session.id;
    drain_writer(app, writer);

    assert_eq!(AppSession::load(id, &dir).unwrap().messages().len(), 1);
}

#[test]
fn reload_leaves_empty_session_unpersisted_on_disk() {
    let (tmp, _dir, writer, mut app) = tempdir_app();
    app.execute_command(cmd("/reload"), 0);
    drain_writer(app, writer);

    let sessions_dir = tmp.path().join(maki_storage::sessions::SESSIONS_DIR);
    let entries = std::fs::read_dir(&sessions_dir)
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(entries, 0);
}

#[test]
fn restore_resumed_session_flushes_queued_messages_and_round_trips() {
    let mut app = test_app();
    app.state.session_mut().meta.queued_messages = vec!["q1".into(), "q2".into()];

    app.restore_resumed_session();
    assert_eq!(app.queue.text_messages(), ["q1", "q2"]);

    app.checkpoint();
    assert_eq!(app.state.session.meta.queued_messages, ["q1", "q2"]);
}

#[test]
fn apply_loaded_session_defers_queued_messages_until_respawn() {
    let mut app = test_app();
    let mut session = AppSession::new("test-model", "/tmp/test");
    session.meta.queued_messages = vec!["deferred".into()];
    session.push_message(Message::user("hello".into()));

    let model = app.state.model.clone();
    app.apply_loaded_session(session, &model);

    assert!(app.queue.is_empty());
    assert_eq!(app.state.session.meta.queued_messages, ["deferred"]);
}

#[test]
fn yolo_toggle() {
    let mut app = test_app();
    assert!(!app.permissions.is_yolo());
    app.execute_command(cmd("/yolo"), 0);
    assert!(app.permissions.is_yolo());
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.contains("enabled"), "flash={flash:?}");
    app.execute_command(cmd("/yolo"), 0);
    assert!(!app.permissions.is_yolo());
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.contains("disabled"), "flash={flash:?}");
}

/// The toggle is session state like mode and thinking, so a checkpoint has to
/// mirror it or a resume silently downgrades the session's permissions.
#[test]
fn checkpoint_mirrors_the_yolo_toggle_into_meta() {
    let mut app = test_app();
    app.checkpoint();
    assert_eq!(app.state.session.meta.yolo, None);

    app.execute_command(cmd("/yolo"), 0);
    app.checkpoint();
    assert_eq!(app.state.session.meta.yolo, Some(true));

    app.execute_command(cmd("/yolo"), 0);
    app.checkpoint();
    assert_eq!(app.state.session.meta.yolo, Some(false));
}

fn session_with_yolo(stored: Option<bool>) -> AppSession {
    let mut session = AppSession::new(TEST_MODEL_SPEC, TEST_CWD);
    session.meta.yolo = stored;
    session.push_message(Message::user(RESUMED_PROMPT.into()));
    session
}

/// The restored permissions, then what the next checkpoint writes back. Both
/// matter: `--yolo` and `always_yolo` are properties of the invocation, so a
/// resume under the flag must neither mark an untouched session nor erase the
/// intent a marked one already carries.
#[test_case(false, None        => (false, None)        ; "no_flag_and_nothing_stored_stays_off")]
#[test_case(true,  None        => (true,  None)        ; "the_flag_applies_without_marking_the_session")]
#[test_case(false, Some(true)  => (true,  Some(true))  ; "stored_on_comes_back_without_the_flag")]
#[test_case(true,  Some(true)  => (true,  Some(true))  ; "the_flag_does_not_wipe_stored_on")]
#[test_case(true,  Some(false) => (false, Some(false)) ; "stored_off_overrides_the_flag")]
fn resume_applies_stored_yolo(seed: bool, stored: Option<bool>) -> (bool, Option<bool>) {
    let mut app = spawned_app(session_with_yolo(stored), test_permissions(seed));

    app.restore_resumed_session();
    app.checkpoint();
    (app.permissions.is_yolo(), app.state.session.meta.yolo)
}

/// `focus_session` sends the same key press down this path instead of a fresh
/// runtime whenever the focused tab is blank and idle, so it has to reach the
/// same permissions as `resume_applies_stored_yolo`.
#[test_case(false, None        => (false, None)        ; "no_flag_and_nothing_stored_stays_off")]
#[test_case(true,  None        => (true,  None)        ; "the_flag_applies_without_marking_the_session")]
#[test_case(false, Some(true)  => (true,  Some(true))  ; "stored_on_comes_back_without_the_flag")]
#[test_case(true,  Some(true)  => (true,  Some(true))  ; "the_flag_does_not_wipe_stored_on")]
#[test_case(true,  Some(false) => (false, Some(false)) ; "stored_off_overrides_the_flag")]
fn loading_a_session_applies_stored_yolo(seed: bool, stored: Option<bool>) -> (bool, Option<bool>) {
    let mut app = spawned_app(
        AppSession::new(TEST_MODEL_SPEC, TEST_CWD),
        test_permissions(seed),
    );
    let model = app.state.model.clone();

    app.apply_loaded_session(session_with_yolo(stored), &model);
    app.checkpoint();
    (app.permissions.is_yolo(), app.state.session.meta.yolo)
}

/// A tab keeps one permission manager for its whole life, so without an
/// explicit reset `/new` would carry the rules the user allowed last time into
/// a session nobody granted them for. The yolo toggle is not one of those. The
/// user set it, like the mode, so it rides along and only the grants go.
#[test_case(false => (true,  Some(true))  ; "a_fresh_session_keeps_the_toggle_on")]
#[test_case(true  => (false, Some(false)) ; "a_fresh_session_keeps_the_toggle_off")]
fn resetting_the_session_drops_what_the_last_one_was_granted(seed: bool) -> (bool, Option<bool>) {
    let mut app = spawned_app(session_with_yolo(Some(!seed)), test_permissions(seed));
    app.permissions.load_session_rules(vec![session_rule()]);
    assert_eq!(app.permissions.is_yolo(), !seed);

    app.reset_session();
    app.checkpoint();

    assert!(app.permissions.session_rules_snapshot().is_empty());
    assert!(app.state.session.meta.session_rules.is_empty());
    (app.permissions.is_yolo(), app.state.session.meta.yolo)
}

#[test]
fn usage_command_toggles_modal() {
    let mut app = test_app();
    assert!(!app.usage_modal.is_open());
    let open_actions = app.execute_command(cmd("/usage"), 0);
    assert!(app.usage_modal.is_open());
    assert!(
        open_actions
            .iter()
            .any(|a| matches!(a, Action::RefreshUsage)),
        "opening should request a quota refresh"
    );
    let close_actions = app.execute_command(cmd("/usage"), 0);
    assert!(!app.usage_modal.is_open());
    assert!(
        !close_actions
            .iter()
            .any(|a| matches!(a, Action::RefreshUsage)),
        "closing should not trigger a refresh"
    );
}

#[test]
fn ctrl_r_refreshes_usage_while_modal_open() {
    let mut app = test_app();
    app.execute_command(cmd("/usage"), 0);
    assert!(app.usage_modal.is_open());

    let actions = app.update(Msg::Key(kb::REFRESH.to_key_event()));
    assert!(
        actions.iter().any(|a| matches!(a, Action::RefreshUsage)),
        "Ctrl+R should emit RefreshUsage"
    );
    assert!(app.usage_modal.is_open(), "modal should stay open");
}

#[test]
fn cd_command_behavior() {
    let mut app = test_app();
    app.execute_command(
        ParsedCommand {
            name: "/cd".into(),
            args: "/tmp".into(),
            bang: false,
        },
        0,
    );
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.starts_with("cd /tmp"), "flash={flash:?}");
    // Use `canonicalize_clean` (resolves symlinks like the OS does) rather
    // than `absolute` which preserves symlinks. On macOS `/tmp` is a symlink
    // to `/private/tmp`; production `cmd_cd` reads back `current_dir()` which
    // returns the resolved form, so the test expectation must match.
    let resolved = maki_storage::paths::canonicalize_clean(Path::new("/tmp"));
    assert_eq!(app.state.session.cwd, resolved.to_string_lossy());

    app.execute_command(
        ParsedCommand {
            name: "/cd".into(),
            args: "/nonexistent_path_12345".into(),
            bang: false,
        },
        0,
    );
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.starts_with("cd: "), "error flash={flash:?}");
}

#[test]
fn typed_slash_command_executes() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, "/help");
    assert!(actions.is_empty());
    assert!(app.help_modal.is_open());
}

const LUA_COMMAND_RAN: &str = "lua command with args must reach the plugin";
const LUA_COMMAND_NOT_SENT: &str = "lua command with args must not reach the model";

/// The palette hides a lua command once the typed words pass its `max_args`,
/// and a hidden command falls through to `handle_submit`, so a multi word
/// `nargs` command must still be routed to its plugin.
#[test]
fn typed_lua_command_with_args_executes() {
    let dir = StateDir::from_path(env::temp_dir());
    let mut app = build_app_with_lua(
        dir.clone(),
        Arc::new(test_writer(dir)),
        LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: "/rename".into(),
            description: "Rename the current session".into(),
            plugin: "sessions".into(),
            max_args: usize::MAX,
        }]),
    );
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;

    let actions = type_and_submit(&mut app, "/rename my title");

    assert!(actions.is_empty(), "{LUA_COMMAND_NOT_SENT}");
    assert!(probe.try_recv().is_some(), "{LUA_COMMAND_RAN}");
}

const RUN_CMDLINE_REJECTED: &str = "a rejected cmdline must not run anything";

#[test_case("/new" ; "plain")]
#[test_case("/NEW" ; "uppercase")]
#[test_case("  /new  " ; "surrounding_whitespace")]
#[test_case("new" ; "missing_slash")]
fn run_cmdline_executes_builtin(cmdline: &str) {
    let mut app = test_app();

    let actions = app.run_cmdline(cmdline, 0).unwrap();

    assert!(matches!(&actions[..], [Action::NewSession]));
}

#[test]
fn run_cmdline_splits_args_off_the_name() {
    let mut app = test_app();

    let actions = app.run_cmdline("/btw what is rust?", 0).unwrap();

    assert!(matches!(&actions[..], [Action::Btw(q)] if q == "what is rust?"));
}

/// Only the typed path clears the input, so a keybind or autocmd reaching for
/// `run_command` cannot eat a half-written message.
#[test]
fn run_cmdline_keeps_typed_input() {
    let mut app = test_app();
    app.input_box.set_input("half written".into());

    app.run_cmdline("/usage", 0).unwrap();

    assert_eq!(app.input_box.buffer.value(), "half written");
}

#[test]
fn run_cmdline_unknown_name_errors_without_dispatching() {
    let mut app = test_app();
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;

    let Err(err) = app.run_cmdline("/nope", 0) else {
        panic!("{RUN_CMDLINE_REJECTED}");
    };

    assert!(err.contains("/nope"), "err={err:?}");
    assert!(probe.try_recv_command().is_none(), "{RUN_CMDLINE_REJECTED}");
}

#[test]
fn run_cmdline_rejects_past_max_depth() {
    let mut app = test_app();

    let Err(err) = app.run_cmdline("/new", crate::app::MAX_COMMAND_DEPTH + 1) else {
        panic!("{RUN_CMDLINE_REJECTED}");
    };

    assert_eq!(err, crate::app::COMMAND_DEPTH_MSG);
    assert!(
        app.run_cmdline("/new", crate::app::MAX_COMMAND_DEPTH)
            .is_ok(),
        "the cap itself must still run"
    );
}

/// A Lua command reached through an alias carries the hop count onward, or a
/// cycle of Lua aliases would never trip the cap. It goes out spelled as
/// registered, since only that spelling dispatches.
#[test]
fn run_cmdline_forwards_depth_to_lua_command() {
    let dir = StateDir::from_path(env::temp_dir());
    let mut app = build_app_with_lua(
        dir.clone(),
        Arc::new(test_writer(dir)),
        LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: "/Sessions".into(),
            description: "Browse sessions".into(),
            plugin: "sessions".into(),
            max_args: 0,
        }]),
    );
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;

    app.run_cmdline("/sessions", 3).unwrap();

    assert_eq!(
        probe.try_recv_command(),
        Some(("/Sessions".to_string(), String::new(), 3))
    );
}

#[test]
fn slash_noncommand_sends_as_prompt() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, "/nonexistent");
    assert!(app.status_bar.flash_text().is_none());
    assert!(actions.iter().any(|a| matches!(a, Action::SendMessage(..))));
}

fn build_rewind_app() -> App {
    let mut app = test_app();

    app.state.session_mut().replace_messages(vec![
        Message::user("first prompt".into()),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "response 1".into(),
                },
                ContentBlock::tool_use("tool-1", "bash", serde_json::json!({})),
            ],
            ..Default::default()
        },
        Message::user("second prompt".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "response 2".into(),
            }],
            ..Default::default()
        },
        Message::user("third prompt".into()),
    ]);
    app.state
        .session_mut()
        .insert_tool_output("tool-1".into(), ToolOutput::Plain("output".into()));
    app
}

fn rewind_to_second_turn() -> RewindEntry {
    RewindEntry {
        turn_index: 2,
        prompt_preview: "2: second".into(),
        prompt_text: "second prompt".into(),
    }
}

#[test]
fn rewind_to_middle_truncates_and_populates_input() {
    let mut app = build_rewind_app();
    let old_run_id = app.run_id;
    let actions = app.rewind_to(rewind_to_second_turn());

    assert_eq!(app.state.session.messages().len(), 2);
    assert!(app.state.session.tool_outputs().contains_key("tool-1"));
    assert_eq!(app.input_box.buffer.value(), "second prompt");
    assert_eq!(app.run_id, old_run_id);

    let Action::LoadSession(ref loaded) = actions[0] else {
        panic!("expected LoadSession");
    };
    assert_eq!(loaded.messages.len(), 2);
}

/// Dropping two short messages may shave a few tokens off the gauge, never the
/// baseline underneath it. A session that never ran a turn has no baseline, so
/// there the rough estimate is all we get.
#[test_case(MEASURED_CONTEXT, MEASURED_CONTEXT - SMALL_HISTORY ; "keeps_measured_baseline")]
#[test_case(0,                0                                ; "falls_back_to_estimate")]
fn rewind_recomputes_context_size(measured: u32, floor: u32) {
    let mut app = build_rewind_app();
    app.state.context_size = measured;
    app.rewind_to(rewind_to_second_turn());

    let size = app.state.context_size;
    assert!(
        size > floor && size < floor + SMALL_HISTORY,
        "context {size} left the {floor}..{} window",
        floor + SMALL_HISTORY
    );
    assert_eq!(app.chats[0].context_size, size);
}

#[test]
fn rewind_to_first_turn_clears_everything() {
    let mut app = build_rewind_app();
    app.state.context_size = MEASURED_CONTEXT;
    app.state.token_usage.input = 500;
    app.state.token_usage.output = 200;
    let entry = RewindEntry {
        turn_index: 0,
        prompt_preview: "1: first".into(),
        prompt_text: "first prompt".into(),
    };
    let actions = app.rewind_to(entry);

    assert!(app.state.session.messages().is_empty());
    assert!(!app.state.session.tool_outputs().contains_key("tool-1"));
    assert_eq!(app.state.token_usage.input, 500);
    assert_eq!(app.state.token_usage.output, 200);
    assert_eq!(app.state.context_size, 0);
    assert_eq!(app.chats[0].context_size, 0);
    assert!(matches!(&actions[0], Action::LoadSession(_)));
}

#[test_case(Duration::ZERO,          true  ; "keeps_fresh_error")]
#[test_case(Duration::from_secs(60), false ; "clears_stale_error")]
fn tick_error_expiry(age: Duration, expect_error: bool) {
    let mut app = test_app();
    app.status = Status::Error {
        message: "fail".into(),
        since: Instant::now() - age,
    };
    let _ = app.tick_error_expiry();
    assert_eq!(matches!(app.status, Status::Error { .. }), expect_error);
}

#[test]
fn retry_clears_in_progress_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolPending {
        id: "t1".into(),
        name: "bash".into(),
    }));
    assert_eq!(app.chats[0].in_progress_count(), 1);

    app.update(agent_msg(AgentEvent::Retry {
        attempt: 1,
        message: "overloaded".into(),
        delay_ms: 1000,
    }));
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(app.retry_info.is_some());
}

#[test]
fn retry_clears_subagent_in_progress_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::ToolPending {
            id: "st1".into(),
            name: "bash".into(),
        },
        TASK_ID,
        Some("research"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[1].in_progress_count(), 1);

    app.update(subagent_msg(
        AgentEvent::Retry {
            attempt: 1,
            message: "overloaded".into(),
            delay_ms: 1000,
        },
        TASK_ID,
        Some("research"),
    ));
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.retry_info.is_none());
}

fn auth_retry_enter(app: &mut App) -> Vec<Action> {
    app.update(Msg::Key(key(KeyCode::Enter)))
}

fn auth_retry_type_then_enter(app: &mut App) -> Vec<Action> {
    type_and_submit(app, "ignored")
}

#[test_case(auth_retry_enter          ; "bare_enter")]
#[test_case(auth_retry_type_then_enter ; "typed_text_then_enter")]
fn auth_retry_sends_empty_answer(submit: fn(&mut App) -> Vec<Action>) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let (tx, rx) = flume::unbounded();
    app.answer_tx = Some(tx);

    app.update(agent_msg(AgentEvent::AuthRequired));
    assert!(matches!(
        app.pending_input,
        PendingInput::AuthRetry { subagent_id: None }
    ));

    let actions = submit(&mut app);
    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(rx.try_recv().unwrap(), "");
}

fn app_with_subagent_tx(id: &str) -> (App, flume::Receiver<String>, flume::Receiver<String>) {
    let (sub_tx, sub_rx) = flume::unbounded();
    let (main_tx, main_rx) = flume::unbounded();
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.answer_tx = Some(main_tx);
    app.update(Msg::Agent(Box::new(Envelope {
        event: AgentEvent::TextDelta { text: "x".into() },
        subagent: Some(subagent_info_with_tx(id, "research", Some(sub_tx))),
        run_id: 1,
    })));
    (app, sub_rx, main_rx)
}

#[test]
fn auth_required_in_subagent_shows_in_both_chats() {
    let mut app = app_with_subagent_id("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    assert_eq!(app.chats[1].last_message_text(), AUTH_EXPIRED_MSG);
    assert_eq!(app.chats[0].last_message_text(), AUTH_EXPIRED_MSG);
    assert!(matches!(
        app.pending_input,
        PendingInput::AuthRetry { subagent_id: Some(ref id) } if id == "sub1"
    ));
}

#[test]
fn auth_retry_in_subagent_routes_to_subagent_channel() {
    let (mut app, sub_rx, main_rx) = app_with_subagent_tx("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(sub_rx.try_recv().unwrap(), "");
    assert!(main_rx.try_recv().is_err());
}

#[test]
fn cancel_clears_subagent_auth_retry() {
    let (mut app, sub_rx, _main_rx) = app_with_subagent_tx("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    cancel_app(&mut app);

    assert_eq!(app.pending_input, PendingInput::None);
    assert!(sub_rx.try_recv().is_err());
}

#[test]
fn stale_auth_required_after_cancel_is_dropped() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 2;
    let count_before = app.chats[0].message_count();
    app.update(Msg::Agent(Box::new(Envelope {
        event: AgentEvent::AuthRequired,
        subagent: None,
        run_id: 1,
    })));
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(app.chats[0].message_count(), count_before);
}

#[test]
fn send_to_agent_unknown_subagent_falls_back_to_main() {
    let (main_tx, main_rx) = flume::unbounded();
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.answer_tx = Some(main_tx);

    app.pending_input = PendingInput::AuthRetry {
        subagent_id: Some("nonexistent".into()),
    };
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(main_rx.try_recv().unwrap(), "");
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test_case(ScrollPos { seg: 4, row: 2 }, false ; "restores_scroll_position")]
#[test_case(ScrollPos::default(),          true  ; "restores_auto_scroll")]
fn search_escape_restores_scroll(scroll: ScrollPos, auto_scroll: bool) {
    let mut app = test_app();
    app.active_chat().restore_scroll(scroll, auto_scroll);

    app.update(Msg::Key(kb::SEARCH.to_key_event()));
    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(!app.search_modal.is_open());
    assert_eq!(app.active_chat().scroll_pos(), scroll);
    assert_eq!(app.active_chat().auto_scroll(), auto_scroll);
}

#[test]
fn mcp_command_opens_picker() {
    let mut app = test_app();
    app.execute_command(cmd("/mcp"), 0);
    assert!(app.mcp_picker.is_open());
}

#[test]
fn mcp_toggle_dispatches_action() {
    let mut app = test_app();
    app.mcp_picker = McpPicker::new(
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![McpServerInfo {
                name: "test-srv".into(),
                transport_kind: "stdio",
                tool_count: 2,
                prompt_count: 0,
                status: McpServerStatus::Running,
                config_path: PathBuf::from("/tmp/config.toml"),
                url: None,
                oauth: None,
            }],
            prompts: vec![],
            pids: vec![],
            generation: 0,
        }),
        McpConfigErrors::new(PathBuf::new()),
    );
    app.execute_command(cmd("/mcp"), 0);

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(
        &actions[0],
        Action::ToggleMcp(name, false) if name == "test-srv"
    ));
}

#[test_case(
    |app: &mut App| { app.state.mode = Mode::Plan; app.plan_form.on_plan_ready(); },
    ""
    ; "consumed_by_plan_form"
)]
#[test_case(
    |app: &mut App| {
        app.state.session_mut().push_message(Message::user("test".into()));
        app.open_rewind_picker();
    },
    ""
    ; "routed_to_open_picker"
)]
#[test_case(
    |app: &mut App| { app.update(Msg::Key(kb::SEARCH.to_key_event())); },
    ""
    ; "routed_to_search_modal"
)]
#[test_case(
    |_: &mut App| {},
    "pasted"
    ; "falls_through_to_input"
)]
fn paste_routing(setup: fn(&mut App), expected_input: &str) {
    let mut app = test_app();
    setup(&mut app);
    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), expected_input);
}

#[test_case(PlanState::None,                                       true  ; "no_plan")]
#[test_case(PlanState::Drafting(PathBuf::from("/tmp/plan.md")),     false ; "plan_drafting")]
#[test_case(PlanState::Ready(PathBuf::from("/tmp/plan.md")),       false ; "plan_ready")]
fn open_editor(plan: PlanState, expect_flash: bool) {
    let mut app = test_app();
    let plan_path = plan.path().map(Path::to_path_buf);
    app.state.plan = plan;
    let actions = app.update(Msg::Key(kb::OPEN_EDITOR.to_key_event()));
    if expect_flash {
        assert!(actions.is_empty());
        assert_eq!(app.status_bar.flash_text().unwrap(), FLASH_NO_PLAN);
        assert!(!app.plan_form.is_visible());
    } else {
        let expected = plan_path.unwrap();
        assert!(matches!(&actions[..], [Action::OpenEditor(p)] if p == &expected));
        assert!(!app.plan_form.is_visible());
    }
}

#[test]
fn alt_o_opens_editor_for_input() {
    let mut app = test_app();
    app.input_box.buffer.insert_text("hello");
    let actions = app.update(Msg::Key(kb::EDIT_INPUT.to_key_event()));
    assert!(matches!(&actions[..], [Action::EditInputInEditor]));
}

#[test]
fn btw_empty_flashes_error() {
    let mut app = test_app();
    let actions = app.execute_command(
        ParsedCommand {
            name: "/btw".into(),
            args: String::new(),
            bang: false,
        },
        0,
    );
    assert!(actions.is_empty());
    assert_eq!(
        app.status_bar.flash_text().unwrap(),
        "Usage: /btw <question>"
    );
}

#[test]
fn btw_with_question_returns_action() {
    let mut app = test_app();
    let actions = app.execute_command(
        ParsedCommand {
            name: "/btw".into(),
            args: "what is rust?".into(),
            bang: false,
        },
        0,
    );
    assert!(matches!(&actions[..], [Action::Btw(q)] if q == "what is rust?"));
}

#[test]
fn btw_modal_key_routing_and_animation() {
    let mut app = test_app();
    let (tx, rx) = flume::bounded(1);
    app.btw_modal.open("test", rx);

    // A pending stream is data, drained by `poll`. Only the typewriter
    // revealing the answer moves on its own.
    assert!(app.btw_modal.is_streaming());
    assert_eq!(app.btw_modal.cadence(), Cadence::IDLE);
    tx.send(BtwEvent::TextDelta("hi".into())).unwrap();
    assert_eq!(app.btw_modal.poll(), Dirty::YES);
    assert_eq!(app.btw_modal.cadence(), Cadence::SMOOTH);

    let actions = app.update(Msg::Key(key(KeyCode::Char('x'))));
    assert!(actions.is_empty());
    assert!(app.btw_modal.is_open());
    assert_eq!(app.input_box.buffer.value(), "");

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert!(!app.btw_modal.is_open());
    assert_eq!(app.btw_modal.cadence(), Cadence::IDLE);
}

#[test]
fn overlay_zone_click_gating() {
    let mut app = test_app();
    let msg = Rect::new(0, 0, 80, 15);
    let overlay = Rect::new(10, 3, 60, 10);
    set_zone(&mut app, SelectionZone::Messages, msg);
    set_zone(&mut app, SelectionZone::Overlay, overlay);
    app.help_modal.toggle();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 1));
    assert!(app.selection_state.is_none());

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 20, 5));
    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.sel().zone, SelectionZone::Overlay);
}

fn streaming_app_with_history() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let history = vec![
        Message::user("hello".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "world".into(),
            }],
            ..Default::default()
        },
    ];
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(
        maki_agent::HistorySnapshot::new(history),
    )));
    app
}

/// The stale event is dropped, yet the cancelled turn still reaches disk: the
/// next frame's checkpoint syncs the mirror whatever event arrived.
#[test_case(done() ; "stale_done")]
#[test_case(
    AgentEvent::Error { message: "timeout".into() } ; "stale_error"
)]
fn checkpoint_after_cancel_persists_the_cancelled_turn(event: AgentEvent) {
    let mut app = streaming_app_with_history();
    let old_run_id = app.run_id;
    cancel_app(&mut app);
    assert_ne!(app.run_id, old_run_id);
    assert!(app.state.session.messages().is_empty());

    app.update(agent_msg_with_run_id(event, old_run_id));
    app.checkpoint();
    assert_eq!(app.state.session.messages().len(), 2);
}

#[test]
fn parent_done_reconciles_unresolved_children_and_tools() {
    let mut app = streaming_app_with_history();
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "task1".into(),
        tool: "task".into(),
        summary: "research".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "child-tool".into(),
            tool: "read".into(),
            summary: "reading".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        "task1",
        Some("research"),
    ));
    let buf = Arc::new(maki_agent::SharedBuf::new());
    app.update(subagent_msg(
        AgentEvent::LiveToolBuf {
            id: "child-tool".into(),
            body: buf,
        },
        "task1",
        None,
    ));

    app.update(done_event());

    assert!(app.chats[1].is_finished());
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(
        app.chats[0]
            .last_message_text()
            .contains(MISSING_TOOL_COMPLETION)
    );
    app.checkpoint();
    assert!(app.state.session.subagents().is_empty());
    assert_eq!(app.state.session.messages().len(), 2);
    assert!(app.state.session.tool_outputs().is_empty());
    assert_eq!(app.cadence(), Cadence::IDLE);
}

#[test]
fn parent_error_refreshes_picker_and_persists_only_completed_children() {
    let mut app = streaming_app_with_history();
    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta { text: "one".into() },
        "task1",
        "first",
        "model-a",
    ));
    finish_subagent(&mut app, "task1", false);
    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta { text: "two".into() },
        "task2",
        "second",
        "model-b",
    ));
    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta {
            text: "three".into(),
        },
        "task3",
        "third",
        "model-c",
    ));
    finish_subagent(&mut app, "task3", false);

    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));

    app.checkpoint();
    let saved: Vec<_> = app
        .state
        .session
        .subagents()
        .iter()
        .map(|subagent| {
            (
                subagent.tool_use_id.as_str(),
                subagent.name.as_str(),
                subagent.model.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        saved,
        vec![
            ("task1", "first", Some("model-a")),
            ("task3", "third", Some("model-c")),
        ]
    );
}

#[test]
fn reserved_shell_survives_parent_done_until_shell_done() {
    let mut app = streaming_app_with_history();
    let id = app.shell.reserve_id();

    app.update(done_event());
    assert!(app.shell.active_ids().contains(&id));

    app.handle_shell_event(shell::ShellEvent::Start {
        id: id.clone(),
        command: "true".into(),
    });
    assert_eq!(app.chats[0].in_progress_count(), 1);
    app.handle_shell_event(shell::ShellEvent::Done {
        id: id.clone(),
        command: "true".into(),
        output: String::new(),
        is_error: false,
        visible: false,
    });
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(!app.shell.active_ids().contains(&id));
}

#[test]
fn active_shell_survives_agent_error_while_agent_and_child_tools_fail() {
    let mut app = streaming_app_with_history();
    let shell_id = app.shell.reserve_id();
    app.handle_shell_event(shell::ShellEvent::Start {
        id: shell_id.clone(),
        command: "true".into(),
    });
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "agent-tool".into(),
        tool: "read".into(),
        summary: "reading".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "child-tool".into(),
            tool: "read".into(),
            summary: "reading".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        "task1",
        Some("research"),
    ));

    app.update(agent_msg(AgentEvent::Error {
        message: "provider overloaded".into(),
    }));

    assert_eq!(app.chats[0].in_progress_count(), 1);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chats[1].is_finished());

    app.handle_shell_event(shell::ShellEvent::Done {
        id: shell_id.clone(),
        command: "true".into(),
        output: String::new(),
        is_error: false,
        visible: false,
    });
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(!app.shell.active_ids().contains(&shell_id));
}

#[test]
fn main_shell_exclusion_does_not_protect_same_id_in_child_chat() {
    let mut app = streaming_app_with_history();
    let id = app.shell.reserve_id();
    app.handle_shell_event(shell::ShellEvent::Start {
        id: id.clone(),
        command: "true".into(),
    });
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: id.clone(),
            tool: "read".into(),
            summary: "reading".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        "task1",
        Some("research"),
    ));

    app.update(done_event());

    assert_eq!(app.chats[0].in_progress_count(), 1);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chats[1].is_finished());
}

#[test]
fn error_event_matching_run_id_saves_session_and_queued_messages() {
    let mut app = streaming_app_with_history();
    app.queue_and_notify(queued_msg("next"));

    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
    app.checkpoint();

    assert_eq!(app.state.session.messages().len(), 2);
    assert_eq!(app.state.session.meta.queued_messages, ["next"]);
    assert!(app.queue.is_empty());

    assert_eq!(app.state.session.meta.queued_messages, ["next"]);

    type_and_submit(&mut app, "replacement");
    app.checkpoint();
    assert!(app.state.session.meta.queued_messages.is_empty());
}

#[test]
fn flush_restored_queue_drops_recovery_snapshot() {
    let mut app = streaming_app_with_history();
    app.queue_and_notify(queued_msg("next"));
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
    app.checkpoint();
    assert_eq!(app.state.session.meta.queued_messages, ["next"]);

    app.flush_restored_queue();
    app.checkpoint();
    assert_eq!(app.state.session.meta.queued_messages, ["next"]);
    assert!(app.recoverable_queue.is_empty());

    app.queue.clear();
    app.checkpoint();
    assert!(app.state.session.meta.queued_messages.is_empty());
}

// --- Plan form integration tests ---

fn implement_msg(parallel: bool) -> String {
    if parallel {
        format!("{IMPLEMENT_MSG_PREFIX} at `test-plan.md`. {IMPLEMENT_PARALLEL_HINT}")
    } else {
        format!("{IMPLEMENT_MSG_PREFIX} at `test-plan.md`.")
    }
}

fn plan_app() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("test-plan.md"));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 42 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
    app
}

#[test_case(Mode::Plan,  true  ; "plan_mode_tooldone_opens_form")]
#[test_case(Mode::Build, false ; "build_mode_tooldone_no_form")]
fn tool_done_write_opens_plan_form(mode: Mode, expect_form: bool) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = mode;
    app.state.plan = PlanState::Drafting(PathBuf::from("/tmp/plans/test.md"));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 42 bytes to /tmp/plans/test.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("/tmp/plans/test.md".into()),
    }))));
    assert_eq!(app.plan_form.is_visible(), expect_form);
    if expect_form {
        assert!(app.state.plan.is_ready());
    }
}

#[test]
fn done_event_does_not_open_plan_form() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("test-plan.md"));
    app.update(done_event());
    assert!(!app.plan_form.is_visible());
}

#[test]
fn re_edit_keeps_plan_form_visible() {
    let mut app = plan_app();
    assert!(app.state.plan.is_ready());
    assert!(app.plan_form.is_visible());

    // Agent edits the plan again (second write to same path) — idempotent, stays Ready
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t2".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 50 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
    assert!(matches!(app.state.plan, PlanState::Ready(_)));
    assert!(app.plan_form.is_visible());
}

#[test_case(1, Mode::Build, true,  true  ; "clear_and_implement")]
#[test_case(2, Mode::Build, false, true  ; "implement_keeps_context")]
fn plan_form_menu_options(
    downs: usize,
    expected_mode: Mode,
    has_new_session: bool,
    has_send_message: bool,
) {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    for _ in 0..downs {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.state.mode, expected_mode);
    assert_eq!(app.state.plan, PlanState::None);
    assert_eq!(
        actions.iter().any(|a| matches!(a, Action::NewSession)),
        has_new_session
    );
    let expected_msg = implement_msg(PlanForm::new().parallel());
    assert_eq!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SendMessage(i) if i.message == expected_msg)),
        has_send_message
    );
}

#[test]
fn plan_form_implement_toggled_parallel() {
    let mut app = plan_app();
    app.update(Msg::Key(key(KeyCode::Char(' '))));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down)));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    let expected_msg = implement_msg(!PlanForm::new().parallel());
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SendMessage(i) if i.message == expected_msg))
    );
}

#[test]
fn plan_form_open_editor() {
    let mut app = plan_app();

    let actions = app.update(Msg::Key(kb::OPEN_EDITOR.to_key_event()));
    assert!(app.plan_form.is_visible());
    assert!(matches!(&actions[..], [Action::OpenEditor(p)] if p == Path::new("test-plan.md")));
}

fn rewrite_plan(app: &mut App) {
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t2".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 99 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
}

fn dismiss_plan_esc(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Esc)));
}

#[test]
fn rewrite_does_not_reopen_after_dismiss() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    dismiss_plan_esc(&mut app);
    assert!(!app.plan_form.is_visible());
    assert!(app.state.plan.is_ready());

    rewrite_plan(&mut app);
    assert!(!app.plan_form.is_visible());
    assert!(app.state.plan.is_ready());
}

#[test]
fn ctrl_t_toggles_plan_form_in_plan_mode() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(!app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(app.plan_form.is_visible());
}

#[test]
fn ctrl_t_noop_when_plan_not_ready() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("test-plan.md"));
    assert!(!app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(!app.plan_form.is_visible());
}

fn install_override(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
) -> maki_lua::test_support::RequestProbe {
    app.keymap_reader = maki_lua::test_support::keymap_reader_with(vec![maki_lua::KeymapEntry {
        key,
        modifiers,
        desc: "plugin override".into(),
        plugin: Arc::from("test-plugin"),
        id: 1,
    }]);
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;
    probe
}

const OVERRIDE_DISPATCHED: &str = "override callback must be dispatched";
const OVERRIDE_NOT_DISPATCHED: &str = "override callback must not be dispatched";

#[test]
fn override_shadows_builtin_ctrl_when_no_overlay_open() {
    let mut app = test_app();
    let probe = install_override(&mut app, kb::HELP.code, kb::HELP.modifiers);

    let actions = app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert!(
        !app.help_modal.is_open(),
        "override must consume the key before the built-in HELP handler runs"
    );
}

#[test]
fn override_shadows_quit_builtin() {
    let mut app = test_app();
    app.status = Status::Idle;
    let probe = install_override(&mut app, kb::QUIT.code, kb::QUIT.modifiers);

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert_eq!(
        app.exit_request,
        ExitRequest::None,
        "override must consume Ctrl+C before the built-in quit handler runs"
    );
}

#[test]
fn override_shadows_tab_mode_toggle() {
    let mut app = test_app();
    let initial_mode = app.state.mode;
    let probe = install_override(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    let actions = app.update(Msg::Key(key(KeyCode::Tab)));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert_eq!(
        app.state.mode, initial_mode,
        "override must consume Tab before the built-in mode toggle runs"
    );
}

#[test]
fn override_shadows_esc_builtin() {
    let mut app = test_app();
    let probe = install_override(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert!(
        app.last_esc.is_none(),
        "override must consume Esc before the built-in esc handler runs"
    );
}

#[cfg(unix)]
#[test]
fn override_does_not_shadow_suspend() {
    let mut app = test_app();
    let probe = install_override(&mut app, kb::SUSPEND.code, kb::SUSPEND.modifiers);

    let actions = app.update(Msg::Key(kb::SUSPEND.to_key_event()));

    assert!(
        actions.iter().any(|a| matches!(a, Action::Suspend)),
        "suspend is non-remappable: override must not shadow Ctrl+Z"
    );
    assert!(probe.try_recv().is_none(), "{OVERRIDE_NOT_DISPATCHED}");
}

#[test]
fn builtin_runs_when_no_override() {
    let mut app = test_app();

    app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(app.help_modal.is_open());
}

#[test]
fn plan_toggle_beats_override_when_open_and_after_dismiss() {
    let mut app = plan_app();
    let probe = install_override(&mut app, kb::PLAN_TOGGLE.code, kb::PLAN_TOGGLE.modifiers);
    assert!(app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(
        !app.plan_form.is_visible(),
        "open plan form must consume Ctrl+T before the override"
    );

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(
        app.plan_form.is_visible(),
        "Ctrl+T must reopen the dismissed plan form despite the override"
    );
    assert!(probe.try_recv().is_none(), "{OVERRIDE_NOT_DISPATCHED}");
}

#[test]
fn streaming_cancel_wins_over_quit_override() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let probe = install_override(&mut app, kb::QUIT.code, kb::QUIT.modifiers);

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert!(
        matches!(&actions[0], Action::CancelAgent { .. }),
        "built-in cancel must win while streaming even when Ctrl+C is overridden"
    );
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(probe.try_recv().is_none(), "{OVERRIDE_NOT_DISPATCHED}");
}

#[test]
fn dead_host_override_falls_back_to_builtin() {
    let mut app = test_app();
    let _probe = install_override(&mut app, kb::HELP.code, kb::HELP.modifiers);
    app.lua_event_handle = maki_lua::EventHandle::disconnected_for_test();

    app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(
        app.help_modal.is_open(),
        "dead lua host must fall back to the built-in HELP handler"
    );
}

#[test]
fn streaming_cancel_wins_over_esc_override() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.status_bar.flash_duration = Duration::from_secs(3600);
    app.last_esc = Some(Instant::now());
    let probe = install_override(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(
        matches!(&actions[0], Action::CancelAgent { .. }),
        "built-in cancel must win while streaming even when Esc is overridden"
    );
    assert_eq!(app.status, Status::Idle);
    assert!(probe.try_recv().is_none(), "{OVERRIDE_NOT_DISPATCHED}");
}

#[test]
fn reset_session_closes_plan_form() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    app.reset_session();
    assert!(!app.plan_form.is_visible());
}

#[test]
fn ctrl_c_closes_overlay_instead_of_quitting() {
    let mut app = test_app();
    app.help_modal.toggle();
    assert!(app.help_modal.is_open());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(!app.help_modal.is_open());
    assert!(actions.is_empty());
}

#[test]
fn bash_prefix_overrides_mode() {
    let mut app = test_app();

    app.input_box.set_input("! ls".into());
    assert_eq!(&*app.mode_label().0, "[BASH]");

    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(
        app.state.mode,
        Mode::Build,
        "tab must not toggle while bash prefix present"
    );

    app.input_box.set_input("ls".into());
    assert_eq!(&*app.mode_label().0, "[BUILD]");
}

#[test]
fn thinking_toggle_cycles_off_adaptive() {
    let mut app = test_app();
    assert_eq!(app.state.thinking, ThinkingConfig::Off);

    app.execute_command(cmd("/thinking"), 0);
    assert_eq!(app.state.thinking, ThinkingConfig::Adaptive);

    app.execute_command(cmd("/thinking"), 0);
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
}

#[test]
fn thinking_explicit_args() {
    let mut app = test_app();

    app.execute_command(
        ParsedCommand {
            name: "/thinking".into(),
            args: "8192".into(),
            bang: false,
        },
        0,
    );
    assert_eq!(app.state.thinking, ThinkingConfig::Budget(8192));

    app.execute_command(
        ParsedCommand {
            name: "/thinking".into(),
            args: "high".into(),
            bang: false,
        },
        0,
    );
    assert_eq!(app.state.thinking, ThinkingConfig::Effort(Effort::High));
}

#[test]
fn thinking_unsupported_model_flashes_error() {
    let mut app = test_app();
    app.state.model.thinking_override = Some(maki_providers::ThinkingSupport::No);

    app.execute_command(cmd("/thinking"), 0);
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
    assert_eq!(app.status_bar.flash_text(), Some(THINKING_UNSUPPORTED_MSG));
}

#[test]
fn package_commands_are_user_only_and_preserve_update_bang() {
    let mut app = test_app();
    let typed = || ParsedCommand {
        name: PACKUPDATE.into(),
        args: PACK_NAME.into(),
        bang: true,
    };

    app.execute_command(typed(), 1);
    assert_eq!(app.exit_request, ExitRequest::None);
    assert_eq!(
        app.status_bar.flash_text().unwrap(),
        format!("{PACKUPDATE}{PACK_USER_ONLY_SUFFIX}")
    );

    let actions = app.execute_command(typed(), 0);
    assert_eq!(app.exit_request, ExitRequest::None);
    let [Action::PreparePack(PackCommand::Update { name, options })] = actions.as_slice() else {
        panic!("expected one package preparation action");
    };
    assert_eq!(name.as_deref(), Some(PACK_NAME));
    assert!(options.force);
}

#[test]
fn invalid_package_command_stays_in_the_current_tui() {
    let mut app = test_app();

    let actions = app.execute_command(
        ParsedCommand {
            name: "/packdel".into(),
            args: "one two".into(),
            bang: false,
        },
        0,
    );

    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert_eq!(app.status_bar.flash_text(), Some(PACKDEL_USAGE));
}

#[test]
fn completed_package_preparation_reports_all_failures_without_exit() {
    let mut app = test_app();
    let report = PackReport {
        failures: vec!["first".into(), "second".into()],
        ..PackReport::default()
    };

    let actions = app.handle_pack_preparation(PackPreparation::Complete(report));

    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert_eq!(app.status_bar.flash_text(), Some(PACK_FAILURES));
}

/// Preparation runs off the event loop, so an agent can raise a permission
/// prompt while the review is already up. The prompt has a tool waiting on it
/// and owns the bottom panel, so it answers first even though it opened last.
#[test]
fn a_pending_permission_prompt_answers_before_the_package_review() {
    let mut app = test_app();
    app.handle_pack_preparation(PackPreparation::Review {
        prompt: PACK_REVIEW_PROMPT.into(),
        plan: PackPlan::default(),
    });
    app.permission_prompt.open(
        "id".into(),
        maki_config::ToolKey::native("bash"),
        vec!["execute".into()],
        None,
    );

    app.update(Msg::Key(KeyEvent::from(KeyCode::Char('y'))));

    assert!(!app.permission_prompt.is_open());
    assert!(app.pack_review.is_open(), "the review waits its turn");
    assert_eq!(app.exit_request, ExitRequest::None);

    app.update(Msg::Key(KeyEvent::from(KeyCode::Char('y'))));

    assert!(!app.pack_review.is_open());
    assert!(matches!(app.exit_request, ExitRequest::Pack(_)));
}

#[test]
fn thinking_restored_from_session_meta() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    let mut session = AppSession::new("test-model", "/tmp/test");
    session.meta.thinking = Some(StoredThinking::Budget { tokens: 4096 });

    let state = SessionState::from_session(
        session,
        &test_model(),
        &storage,
        &maki_config::ModelPolicy::default(),
    );
    assert_eq!(state.thinking, ThinkingConfig::Budget(4096));
}

fn set_opus_model(app: &mut App) {
    app.state.model = maki_providers::Model::from_spec(OPUS_SPEC).unwrap();
}

#[test]
fn fast_toggle_on_off_on_opus() {
    let mut app = test_app();
    set_opus_model(&mut app);
    assert!(!app.state.fast);

    app.execute_command(cmd("/fast"), 0);
    assert!(app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_ON_MSG));

    app.execute_command(cmd("/fast"), 0);
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_OFF_MSG));
}

#[test]
fn workflow_toggle_flows_into_agent_input() {
    let mut app = test_app();
    let msg = QueuedMessage {
        text: "hi".into(),
        images: Vec::new(),
    };
    assert!(!app.build_agent_input(&msg).workflow);

    app.execute_command(cmd("/workflow"), 0);
    assert!(app.build_agent_input(&msg).workflow);
    assert_eq!(app.status_bar.flash_text(), Some(WORKFLOW_ON_MSG));

    app.execute_command(cmd("/workflow"), 0);
    assert!(!app.build_agent_input(&msg).workflow);
    assert_eq!(app.status_bar.flash_text(), Some(WORKFLOW_OFF_MSG));
}

/// Workflow sessions have synthetic ids that no ToolDone matches, so
/// SubagentHistory is what finishes their chat.
#[test]
fn subagent_history_finishes_workflow_chat() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "sub".into() },
        "session-abc",
        Some("researcher"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert!(!app.chats[1].is_finished());

    app.update(agent_msg_with_run_id(
        AgentEvent::SubagentHistory {
            tool_use_id: "session-abc".into(),
            messages: vec![],
        },
        1,
    ));
    assert!(app.chats[1].is_finished());
    assert_eq!(app.chats[1].last_message_text(), DONE_TEXT);
}

#[test_case(SONNET_SPEC ; "non_opus_anthropic")]
#[test_case("openai/gpt-5.5" ; "non_anthropic")]
fn fast_flashes_error_on_ineligible_model(spec: &str) {
    let mut app = test_app();
    app.state.model = maki_providers::Model::from_spec(spec).unwrap();

    app.execute_command(cmd("/fast"), 0);
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_UNSUPPORTED_MSG));
}

#[test]
fn fast_restored_from_session_meta() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    let mut session = AppSession::new("anthropic/claude-opus-4-8", "/tmp/test");
    session.meta.fast = true;

    let state = SessionState::from_session(
        session,
        &test_model(),
        &storage,
        &maki_config::ModelPolicy::default(),
    );
    assert!(state.fast);
}

#[test]
fn fast_normalized_off_when_restored_onto_ineligible_model() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    // Saved as fast=true, but sonnet cannot do fast mode, so restoring must drop
    // it to false or the UI would show a phantom [fast] badge.
    let mut session = AppSession::new(SONNET_SPEC, "/tmp/test");
    session.meta.fast = true;

    let state = SessionState::from_session(
        session,
        &test_model(),
        &storage,
        &maki_config::ModelPolicy::default(),
    );
    assert!(!state.fast);
}

#[test]
fn model_state_reports_the_model_and_what_it_supports() {
    let mut app = test_app();
    app.state.model = maki_providers::Model::from_spec(PLAIN_MODEL_SPEC).unwrap();
    assert_eq!(
        app.model_state(),
        serde_json::json!({
            "spec": PLAIN_MODEL_SPEC,
            "id": "qwen3",
            "provider": "ollama",
            "thinking": "off",
            "fast": false,
            "supports_thinking": false,
            "supports_fast": false,
        })
    );

    set_opus_model(&mut app);
    app.set_thinking("high").unwrap();
    app.set_fast(true).unwrap();
    assert_eq!(
        app.model_state(),
        serde_json::json!({
            "spec": OPUS_SPEC,
            "id": "claude-opus-4-8",
            "provider": "anthropic",
            "thinking": "high",
            "fast": true,
            "supports_thinking": true,
            "supports_fast": true,
        })
    );
}

/// A plugin redraws its badge from the payload alone, and only when the model
/// really moved: the catalog fetch re-stores the running model once it learns
/// its context window, and startup has nothing to announce yet.
#[test]
fn model_change_fires_once_per_real_swap() {
    let (_tmp, _storage, _writer, mut app) = tempdir_app();
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;
    let before = app.state.model.spec();

    app.emit_model_change();
    assert_eq!(probe.try_recv_autocmd(), None);

    app.update_model(&maki_providers::Model::from_spec(OPUS_SPEC).unwrap());
    app.emit_model_change();
    app.emit_model_change();

    let (event, data) = probe.try_recv_autocmd().expect(MODEL_CHANGED_EVENT);
    assert_eq!(event, MODEL_CHANGED_EVENT);
    assert_eq!(
        data["session_id"],
        serde_json::json!(app.state.session.id.to_string())
    );
    assert_eq!(data["model"], app.model_state());
    assert_eq!(data["model"]["spec"], serde_json::json!(OPUS_SPEC));
    assert_eq!(data["previous_spec"], serde_json::json!(before));
    assert_eq!(probe.try_recv_autocmd(), None);
}

/// Loading a session swaps the whole state in instead of going through
/// `update_model`, so the diff has to catch that one on its own.
#[test]
fn loading_a_session_on_another_model_announces_the_swap() {
    let (_tmp, _storage, _writer, mut app) = tempdir_app();
    let (handle, probe) = maki_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;
    let fallback = app.state.model.clone();

    app.apply_loaded_session(AppSession::new(OPUS_SPEC, "/tmp/test"), &fallback);
    app.emit_model_change();

    let (event, data) = probe.try_recv_autocmd().expect(MODEL_CHANGED_EVENT);
    assert_eq!(event, MODEL_CHANGED_EVENT);
    assert_eq!(data["model"]["spec"], serde_json::json!(OPUS_SPEC));
}

/// What `model_state` reports has to parse back into the same state, or a
/// `maki.model.get` -> `maki.model.set` hop would silently change it.
#[test_case(ThinkingConfig::Off, "off" ; "off")]
#[test_case(ThinkingConfig::Adaptive, "adaptive" ; "adaptive")]
#[test_case(ThinkingConfig::Effort(Effort::High), "high" ; "effort")]
#[test_case(ThinkingConfig::Budget(8192), "8192" ; "budget")]
fn model_state_thinking_round_trips_into_set_thinking(thinking: ThinkingConfig, expected: &str) {
    let mut app = test_app();
    app.state.thinking = thinking;

    let reported = app.model_state()["thinking"].as_str().unwrap().to_owned();
    assert_eq!(reported, expected);
    assert_eq!(app.set_thinking(&reported).unwrap(), thinking);
    assert_eq!(app.set_thinking(&reported).unwrap(), thinking);
}

#[test]
fn set_thinking_toggles_on_blank_input() {
    let mut app = test_app();
    assert_eq!(app.set_thinking("").unwrap(), ThinkingConfig::Adaptive);
    assert_eq!(app.set_thinking("").unwrap(), ThinkingConfig::Off);
}

#[test_case(true, "garbage", THINKING_USAGE ; "unknown_word")]
#[test_case(true, "0", THINKING_USAGE ; "zero_budget")]
#[test_case(false, "low", THINKING_UNSUPPORTED_MSG ; "model_without_thinking")]
fn set_thinking_keeps_state_on_rejected_input(supported: bool, input: &str, expected: &str) {
    let mut app = test_app();
    app.set_thinking("high").unwrap();
    if !supported {
        app.state.model.thinking_override = Some(maki_providers::ThinkingSupport::No);
    }

    assert_eq!(app.set_thinking(input).unwrap_err(), expected);
    assert_eq!(app.state.thinking, ThinkingConfig::Effort(Effort::High));
}

/// Fast must never get stuck on: after switching to a model without fast mode,
/// you still have to be able to turn it off.
#[test]
fn fast_turns_off_on_a_model_that_lost_fast_support() {
    let mut app = test_app();
    set_opus_model(&mut app);
    app.execute_command(cmd("/fast"), 0);
    assert!(app.state.fast);

    app.state.model = maki_providers::Model::from_spec(SONNET_SPEC).unwrap();
    app.execute_command(cmd("/fast"), 0);
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_OFF_MSG));
}

#[test]
fn update_model_to_ineligible_resets_fast() {
    let mut app = test_app();
    set_opus_model(&mut app);
    app.state.fast = true;

    let sonnet = maki_providers::Model::from_spec(SONNET_SPEC).unwrap();
    app.state.update_model(&sonnet);
    assert!(!app.state.fast);
}

#[test]
fn agent_error_creates_synthetic_tool_done_with_message() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "t1".into(),
        tool: "bash".into(),
        summary: "echo hello".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    assert_eq!(app.main_chat().in_progress_count(), 1);

    let error_msg = "Provider is overloaded";
    app.update(agent_msg(AgentEvent::Error {
        message: error_msg.into(),
    }));

    assert_eq!(app.main_chat().in_progress_count(), 0);
    let text = app.main_chat().last_message_text();
    assert!(
        text.contains(error_msg),
        "tool output should contain error: {text}"
    );
}

#[test]
fn ctrl_c_denies_permission_prompt() {
    let mut app = test_app();
    app.permission_prompt.open(
        "id".into(),
        maki_config::ToolKey::native("bash"),
        vec!["execute".into()],
        None,
    );
    assert!(app.permission_prompt.is_open());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(!app.permission_prompt.is_open());
    assert!(actions.is_empty());
}

const TEST_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 40,
};
const SPLIT_EXTENT: u16 = 8;

fn open_split_window(app: &mut App, dir: maki_lua::Split) {
    let buf = Arc::new(maki_agent::SharedBuf::new());
    let config = maki_lua::FloatConfig {
        width: maki_lua::Dimension::Abs(SPLIT_EXTENT),
        height: maki_lua::Dimension::Abs(SPLIT_EXTENT),
        border: maki_lua::Border::None,
        split: dir,
        ..maki_lua::FloatConfig::default()
    };
    let (event_tx, _event_rx) = flume::bounded::<maki_lua::WinEvent>(8);
    let (_cmd_tx, cmd_rx) = flume::bounded::<maki_lua::WinCommand>(8);
    app.float_mgr.open(buf, config, true, event_tx, cmd_rx);
}

#[test]
fn attention_float_marks_app_as_awaiting_input_until_close() {
    let mut app = test_app();
    let buf = Arc::new(maki_agent::SharedBuf::new());
    let config = maki_lua::FloatConfig {
        needs_input: true,
        ..maki_lua::FloatConfig::default()
    };
    let (event_tx, _event_rx) = flume::bounded::<maki_lua::WinEvent>(8);
    let (cmd_tx, cmd_rx) = flume::bounded::<maki_lua::WinCommand>(8);

    app.float_mgr.open(buf, config, true, event_tx, cmd_rx);
    assert!(app.awaiting_input());
    assert_eq!(app.attention(), Some(Notification::QuestionRequested));

    cmd_tx
        .send(maki_lua::WinCommand::SetVisible(false))
        .unwrap();
    let _ = app.float_mgr.tick();
    assert!(!app.awaiting_input());
    assert_eq!(app.attention(), None);

    cmd_tx.send(maki_lua::WinCommand::SetVisible(true)).unwrap();
    let _ = app.float_mgr.tick();
    assert_eq!(app.attention(), Some(Notification::QuestionRequested));

    cmd_tx.send(maki_lua::WinCommand::Close).unwrap();
    let _ = app.float_mgr.tick();
    assert!(!app.awaiting_input());
    assert_eq!(app.attention(), None);
}

#[test]
fn below_split_reserves_bottom_and_suppresses_input() {
    let mut app = test_app();
    let (msg_before, _b, _s, input_before, splits_before) = app.layout_geometry(TEST_AREA);
    assert!(
        splits_before.rect(maki_lua::Split::Below).is_none(),
        "no split open yet"
    );
    assert!(input_before.height > 0, "input box visible before split");

    open_split_window(&mut app, maki_lua::Split::Below);
    let (msg_after, _bottom, _s, input_after, splits_after) = app.layout_geometry(TEST_AREA);

    let band = splits_after
        .rect(maki_lua::Split::Below)
        .expect("below split should reserve a bottom band");
    assert_eq!(
        band.height, SPLIT_EXTENT,
        "below band reserves the requested rows",
    );
    assert!(
        msg_after.height < msg_before.height,
        "chat must shrink to make room for the below split",
    );
    assert_eq!(
        input_after.height, 0,
        "input box is suppressed under a below split"
    );
}

/// `carve` already tests the per-direction geometry; this pins the app wiring:
/// a split shrinks the chat while the full-width status bar stays put. Below is
/// tested separately since it also hides the input box.
#[test_case(maki_lua::Split::Above ; "above")]
#[test_case(maki_lua::Split::Left ; "left")]
#[test_case(maki_lua::Split::Right ; "right")]
fn non_below_split_reserves_band_and_keeps_status_full_width(dir: maki_lua::Split) {
    let mut app = test_app();
    let (msg_before, _b, _s, _i, _sp) = app.layout_geometry(TEST_AREA);

    open_split_window(&mut app, dir);
    let (msg_after, _bottom, status_after, _input, splits) = app.layout_geometry(TEST_AREA);

    assert!(splits.rect(dir).is_some(), "split must reserve a band");
    assert!(
        msg_after.area() < msg_before.area(),
        "chat must shrink to make room for the split",
    );
    assert_eq!(
        status_after.width, TEST_AREA.width,
        "status bar stays full width regardless of the split",
    );
}

#[test]
fn closing_split_restores_layout() {
    let mut app = test_app();
    let before = app.layout_geometry(TEST_AREA);

    open_split_window(&mut app, maki_lua::Split::Below);
    app.float_mgr.close_all();

    let after = app.layout_geometry(TEST_AREA);
    assert_eq!(after, before, "closing the split restores the layout");
}

#[test]
fn permission_prompt_takes_bottom_precedence_over_below_split() {
    let mut app = test_app();
    open_split_window(&mut app, maki_lua::Split::Below);
    open_split_window(&mut app, maki_lua::Split::Left);
    open_split_window(&mut app, maki_lua::Split::Above);
    app.permission_prompt.open(
        "perm-1".into(),
        maki_config::ToolKey::native("bash"),
        vec!["ls".into()],
        None,
    );

    let (_msg, _bottom, _status, _input, splits) = app.layout_geometry(TEST_AREA);
    assert!(
        splits.rect(maki_lua::Split::Below).is_none(),
        "below split must yield the bottom area to an open permission prompt",
    );
    assert!(
        splits.rect(maki_lua::Split::Left).is_some(),
        "the prompt must leave a left split untouched",
    );
    assert!(
        splits.rect(maki_lua::Split::Above).is_some(),
        "the prompt must leave an above split untouched",
    );
}

fn app_with_active_subagent() -> App {
    let mut app = app_with_subagent();
    app.run_builtin(BuiltinAction::NextChat);
    assert_eq!(app.active_chat, 1);
    app
}

#[test]
fn double_esc_in_subagent_cancels_subagent() {
    let mut app = app_with_active_subagent();
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::CancelSubagent { tool_use_id } if tool_use_id == TASK_ID
    ));
    assert!(app.chats[1].is_finished());
    assert_eq!(app.chats[1].last_message_text(), CANCELLED_TEXT);
}

#[test]
fn single_or_stale_esc_in_subagent_flashes() {
    let mut app = app_with_active_subagent();
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert_eq!(app.status_bar.flash_text().unwrap(), FLASH_CANCEL);

    app.last_esc = Some(Instant::now().checked_sub(Duration::from_secs(10)).unwrap());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert!(!app.chats[1].is_finished());
}

#[test]
fn esc_in_main_chat_with_active_subagent_no_cancel() {
    let mut app = app_with_subagent();
    assert_eq!(app.active_chat, 0);
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert!(!matches!(&actions[0], Action::CancelSubagent { .. }));
}

#[test]
fn cancel_subagent_removes_answer_sender() {
    let (mut app, _sub_rx, _main_rx) = app_with_subagent_tx(TASK_ID);
    assert!(!app.subagent_answers.is_empty());
    app.run_builtin(BuiltinAction::NextChat);
    assert_eq!(app.active_chat, 1);
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.subagent_answers.contains_key(TASK_ID));
}

#[test]
fn multiple_subagents_cancel_one_other_unaffected() {
    let mut app = app_with_subagent_id(TASK_ID);
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "y".into() },
        "task2",
        Some("build"),
    ));
    assert_eq!(app.chats.len(), 3);

    app.active_chat = *app.chat_index.get("task2").unwrap();
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::CancelSubagent { tool_use_id } if tool_use_id == "task2"
    ));
    let task1_idx = *app.chat_index.get(TASK_ID).unwrap();
    assert!(!app.chats[task1_idx].is_finished());
    assert!(app.chats[app.active_chat].is_finished());
}

#[test]
fn double_esc_in_finished_subagent_noop() {
    let mut app = app_with_active_subagent();
    finish_subagent_task(&mut app, false);
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
}

#[test]
fn subagent_cancel_then_navigate_back_main_unaffected() {
    let mut app = app_with_active_subagent();
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.chats[1].is_finished());

    app.run_builtin(BuiltinAction::PrevChat);
    assert_eq!(app.active_chat, 0);
    assert_eq!(app.status, Status::Streaming);
    assert!(!app.chats[0].is_finished());
}

// -- Every frame checkpoints: one way in for a history, one trigger to save --

/// Long enough that a waiting change is still waiting when the assert runs, on
/// any machine, so none of these tests depend on the wall clock.
const SOFT_DELAY_HELD: Duration = Duration::from_secs(3600);
const MID_BATCH_RESULT: &str = "file contents";
const TYPED_DRAFT: &str = "hi";
const UNSENT_DRAFT: &str = "half typed thought";
const LIVE_AGENT_TEXT: &str = "live agent turn";
const STORED_SESSION_TEXT: &str = "other session talk";
const SWITCHED_DRAFT: &str = "draft typed after switching";
const BUMP_TITLE: &str = "title bump ";
const TOOL_IDS: [&str; 2] = ["tool-a", "tool-b"];
const FINISHED_TASK_ID: &str = "task-finished";
const UNFINISHED_TASK_ID: &str = "task-unfinished";

#[test]
fn turn_response_normalizes_text_and_truncates_unicode() {
    let long = "界".repeat(201);
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "  first\n\tsecond ".into(),
            },
            ContentBlock::Thinking {
                thinking: "ignored".into(),
                signature: None,
            },
            ContentBlock::Text { text: long },
        ],
        ..Default::default()
    };
    let response = turn_response(&message).unwrap();
    assert_eq!(response.chars().count(), 200);
    assert!(response.starts_with("first second 界"));
    assert_eq!(turn_response(&Message::default()), None);
    assert_eq!(turn_response(&tool_use_msg("tool")), None);
}

#[test]
fn turn_response_stops_after_bounded_large_input() {
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: format!("first {}", "x".repeat(1_000_000)),
            },
            ContentBlock::Text {
                text: "not reached".into(),
            },
        ],
        ..Default::default()
    };

    let response = turn_response(&message).unwrap();

    assert_eq!(response.chars().count(), 200);
    assert!(response.starts_with("first "));
    assert!(!response.contains("not reached"));
}

#[test_case(Notification::TurnComplete { response: Some("answer".into()) }, "answer", false ; "turn_response")]
#[test_case(Notification::TurnComplete { response: None }, "Agent turn complete", false ; "turn_fallback")]
#[test_case(Notification::PermissionRequested { tool: Some("bash".into()) }, "Permission requested: bash", true ; "permission_tool")]
#[test_case(Notification::PermissionRequested { tool: None }, "Permission requested", true ; "permission_fallback")]
#[test_case(Notification::AuthenticationRequired, "Authentication required", true ; "authentication")]
#[test_case(Notification::QuestionRequested, "Question requested", true ; "question")]
#[test_case(Notification::PlanReady, "Plan ready", true ; "plan")]
#[test_case(Notification::error_completion(), "Agent stopped with an error", false ; "error_completion")]
fn notification_message_and_urgency(
    notification: Notification,
    expected_message: &str,
    urgent: bool,
) {
    assert_eq!(notification.message(), expected_message);
    assert_eq!(notification.is_urgent(), urgent);
}

#[test]
fn attention_prioritizes_permission_and_normalizes_tool() {
    let mut app = test_app();
    app.pending_input = PendingInput::AuthRetry { subagent_id: None };
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.plan_form.on_plan_ready();
    app.permission_prompt.open(
        "id".into(),
        maki_config::ToolKey::native("bash"),
        vec!["execute".into()],
        None,
    );
    assert_eq!(
        app.attention(),
        Some(Notification::PermissionRequested {
            tool: Some("bash".into())
        })
    );

    app.permission_prompt
        .open("id".into(), maki_config::ToolKey::Wildcard, vec![], None);
    assert_eq!(
        app.attention(),
        Some(Notification::PermissionRequested { tool: None })
    );
}

#[test]
fn attention_classifies_auth_and_ready_plan() {
    let mut app = test_app();
    app.pending_input = PendingInput::AuthRetry { subagent_id: None };
    assert_eq!(app.attention(), Some(Notification::AuthenticationRequired));

    app.pending_input = PendingInput::None;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.plan_form.on_plan_ready();
    app.status = Status::Streaming;
    assert_eq!(app.attention(), None);
    app.status = Status::Idle;
    assert_eq!(app.attention(), Some(Notification::PlanReady));
    assert!(!app.awaiting_input());

    app.plan_form.hide();
    assert_eq!(app.attention(), None);
    app.plan_form.on_plan_ready();
    app.state.plan = PlanState::Drafting(PathBuf::from("plan.md"));
    assert_eq!(app.attention(), None);
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.state.mode = Mode::Build;
    assert_eq!(app.attention(), None);
}

fn tool_use_msg(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use(id, "read", serde_json::json!({}))],
        ..Default::default()
    }
}

fn tool_result_msg(id: &str, text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: text.into(),
            is_error: false,
        }],
        display_text: Some(String::new()),
        ..Default::default()
    }
}

fn tool_text(id: &str) -> String {
    format!("output of {id}")
}

fn attach_live_history(app: &mut App, messages: Vec<Message>) -> maki_agent::History {
    let mirror: maki_agent::SharedMessages =
        Arc::new(ArcSwap::from_pointee(maki_agent::HistorySnapshot::default()));
    let history = maki_agent::History::new(messages).with_mirror(Arc::clone(&mirror));
    app.shared_history = Some(mirror);
    history
}

/// Types [`TYPED_DRAFT`] one key per frame and hands back the stamp of the
/// write the first key caused. The soft delay never elapses, so every key after
/// the first is still waiting when the caller looks.
fn type_draft_leaving_last_key_waiting(app: &mut App) -> Sent {
    let mut keys = TYPED_DRAFT.chars();
    app.update(Msg::Key(key(KeyCode::Char(keys.next().unwrap()))));
    app.checkpoint();
    let first = app
        .last_sent
        .clone()
        .expect("the first keystroke puts the session on disk");

    for c in keys {
        app.update(Msg::Key(key(KeyCode::Char(c))));
        app.checkpoint_with(SOFT_DELAY_HELD);
    }
    first
}

/// Checkpointing mid-batch used to freeze the tools as failed forever. The
/// synthetic closing message made the snapshot as long as the real results that
/// followed, so the append cursor never saw them.
#[test]
fn mid_batch_checkpoint_does_not_shadow_the_real_tool_results() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let mut history = attach_live_history(
        &mut app,
        vec![Message::user("go".into()), tool_use_msg("t1")],
    );
    app.checkpoint();

    history.push(tool_result_msg("t1", MID_BATCH_RESULT));
    app.checkpoint();

    let id = app.state.session.id;
    drain_writer(app, writer);

    let loaded = AppSession::load(id, &dir).unwrap();
    assert_eq!(loaded.messages().len(), 3);
    let [
        ContentBlock::ToolResult {
            content, is_error, ..
        },
    ] = &loaded.messages()[2].content[..]
    else {
        panic!("expected one real tool result: {:?}", loaded.messages()[2]);
    };
    assert_eq!((content.as_str(), *is_error), (MID_BATCH_RESULT, false));
}

/// In the window between a rewind and the agent respawn, syncing from the
/// mirror would bring back the messages that were just dropped.
#[test]
fn checkpoint_after_rewind_persists_the_truncated_history() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let _live = attach_live_history(
        &mut app,
        vec![
            Message::user("first prompt".into()),
            Message::user("second prompt".into()),
        ],
    );
    app.checkpoint();

    let entry = RewindEntry {
        turn_index: 1,
        prompt_preview: "2: second".into(),
        prompt_text: "second prompt".into(),
    };
    app.rewind_to(entry);
    assert!(app.shared_history.is_none(), "mirror handle is dropped");
    app.checkpoint();

    let id = app.state.session.id;
    drain_writer(app, writer);
    assert_eq!(AppSession::load(id, &dir).unwrap().messages().len(), 1);
}

#[test]
fn reset_session_never_writes_the_old_conversation_under_the_new_id() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let _live = attach_live_history(&mut app, vec![Message::user("old talk".into())]);
    app.checkpoint();
    let old_id = app.state.session.id;

    app.reset_session();
    app.checkpoint();
    let new_id = app.state.session.id;
    assert_ne!(new_id, old_id);

    drain_writer(app, writer);
    assert_eq!(AppSession::load(old_id, &dir).unwrap().messages().len(), 1);
    assert!(
        AppSession::load(new_id, &dir).is_err(),
        "an empty session has no content to persist",
    );
}

/// Two traps in one switch. `install_local_history` has to drop the mirror
/// handle, or the old agent's messages land under the freshly loaded id. And
/// `revision` is `#[serde(skip)]`, so the loaded session starts back at zero and
/// can collide with the revision already sent for the one it replaced, which
/// only keying `last_sent` by id survives.
#[test]
fn load_session_persists_the_new_session_and_leaks_no_history_into_it() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let mut stored = AppSession::new("test-model", "/tmp/test");
    stored.push_message(Message::user(STORED_SESSION_TEXT.into()));
    stored.save(&dir).unwrap();

    let _live = attach_live_history(&mut app, vec![Message::user(LIVE_AGENT_TEXT.into())]);
    app.input_box.set_input(UNSENT_DRAFT.into());
    app.checkpoint();
    let (live_id, sent_revision) = (app.state.session.id, app.state.session.revision());

    app.load_session(stored.id);
    assert_eq!(app.state.session.id, stored.id);
    // Walk the loaded session up to the revision already sent for the live one,
    // so the checkpoint below lands on the exact collision.
    let session = app.state.session_mut();
    while session.revision() + 1 < sent_revision {
        session.set_title(format!("{BUMP_TITLE}{}", session.revision()));
    }
    app.input_box.set_input(SWITCHED_DRAFT.into());
    app.checkpoint();
    assert_eq!(
        app.state.session.revision(),
        sent_revision,
        "both sessions must sit at the same revision for this to test anything"
    );

    drain_writer(app, writer);
    let loaded = AppSession::load(stored.id, &dir).unwrap();
    assert_eq!(loaded.meta.input_draft.as_deref(), Some(SWITCHED_DRAFT));
    assert_eq!(loaded.messages().len(), 1);
    assert_eq!(loaded.messages()[0].user_text(), Some(STORED_SESSION_TEXT));
    let previous = AppSession::load(live_id, &dir).unwrap();
    assert_eq!(previous.messages()[0].user_text(), Some(LIVE_AGENT_TEXT));
}

#[test]
fn idle_checkpoint_changes_nothing() {
    let mut app = test_app();
    app.state
        .session_mut()
        .push_message(Message::user("hello".into()));
    app.checkpoint();
    let (revision, updated_at) = (app.state.session.revision(), app.state.session.updated_at);

    app.checkpoint();
    app.checkpoint();

    assert_eq!(app.state.session.revision(), revision);
    assert_eq!(app.state.session.updated_at, updated_at);
}

/// Issue #675: a crash between a keystroke and submit threw the draft away,
/// because nothing was written until the turn ended. The first key lands within
/// a frame now, and the keys behind it ride along on a later write rather than
/// each costing an `fsync`.
#[test]
fn first_draft_keystroke_lands_and_the_rest_coalesce() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let first = type_draft_leaving_last_key_waiting(&mut app);
    assert_eq!(
        app.last_sent.as_ref(),
        Some(&first),
        "a keystroke on its own waits instead of costing a write",
    );

    app.checkpoint_with(Duration::ZERO);
    assert_ne!(
        app.last_sent.as_ref(),
        Some(&first),
        "and lands once the delay is up"
    );

    let id = app.state.session.id;
    drain_writer(app, writer);
    let saved = AppSession::load(id, &dir).unwrap();
    assert_eq!(saved.meta.input_draft.as_deref(), Some(TYPED_DRAFT));
    assert!(saved.messages().is_empty());
}

#[test]
fn a_content_change_writes_the_waiting_draft_with_it() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    type_draft_leaving_last_key_waiting(&mut app);

    app.state
        .session_mut()
        .push_message(Message::user(LIVE_AGENT_TEXT.into()));
    app.checkpoint_with(SOFT_DELAY_HELD);

    let id = app.state.session.id;
    drain_writer(app, writer);
    let saved = AppSession::load(id, &dir).unwrap();
    assert_eq!(saved.messages().len(), 1, "content never waits");
    assert_eq!(saved.meta.input_draft.as_deref(), Some(TYPED_DRAFT));
}

#[test]
fn shutdown_writes_a_draft_that_is_still_waiting() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    type_draft_leaving_last_key_waiting(&mut app);

    app.checkpoint_now();

    let id = app.state.session.id;
    drain_writer(app, writer);
    let saved = AppSession::load(id, &dir).unwrap();
    assert_eq!(saved.meta.input_draft.as_deref(), Some(TYPED_DRAFT));
}

/// Submitting empties the draft a frame before the agent mirrors the prompt
/// back. Delete the session in that gap and the user loses the one they were
/// just starting.
#[test]
fn submitting_the_draft_keeps_the_session_on_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    type_draft_leaving_last_key_waiting(&mut app);
    let id = app.state.session.id;

    app.update(Msg::Key(key(KeyCode::Enter)));
    app.checkpoint();
    assert!(!app.has_content(), "the submit window is what this covers");

    drain_writer(app, writer);
    assert!(AppSession::load(id, &dir).is_ok());
}

/// The draft put the session on disk, and deleting it leaves nothing worth
/// keeping. Without the delete the file survives with the abandoned draft in
/// it, and the picker offers an empty session to resume.
#[test]
fn deleting_the_draft_takes_the_session_off_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    type_draft_leaving_last_key_waiting(&mut app);
    let id = app.state.session.id;

    for _ in TYPED_DRAFT.chars() {
        app.update(Msg::Key(key(KeyCode::Backspace)));
    }
    app.checkpoint();
    assert!(app.last_sent.is_none(), "nothing is on disk to stamp");

    drain_writer(app, writer);
    assert!(AppSession::load(id, &dir).is_err());
}

/// The second result goes through the append cursor the first one opened, so a
/// stale cursor would quietly drop or duplicate it.
#[test]
fn two_tool_results_checkpointed_separately_both_reach_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.state
        .session_mut()
        .push_message(Message::user("prompt".into()));
    app.status = Status::Streaming;
    app.run_id = 1;

    for tool_id in TOOL_IDS {
        app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: tool_id.into(),
            tool: "bash".into(),
            output: ToolOutput::Plain(tool_text(tool_id).into()),
            is_error: false,
            annotation: None,
            written_path: None,
        }))));
        app.checkpoint();
    }

    let id = app.state.session.id;
    drain_writer(app, writer);
    let loaded = AppSession::load(id, &dir).unwrap();
    for tool_id in TOOL_IDS {
        match loaded.tool_outputs().get(tool_id).map(Arc::as_ref) {
            Some(ToolOutput::Plain(out)) => assert_eq!(out.text, tool_text(tool_id)),
            other => panic!("missing plain output for {tool_id}: {other:?}"),
        }
    }
}

/// The `Done` path clears `chat_index` right after pruning it, so nothing can
/// rebuild the tabs later. Only the `sync_subagents` call inside
/// `retain_resolved_subagents` carries the survivors over.
#[test]
fn turn_end_keeps_only_the_subagents_that_finished() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    for (task_id, name) in [
        (FINISHED_TASK_ID, "finished child"),
        (UNFINISHED_TASK_ID, "open child"),
    ] {
        app.update(subagent_msg(
            AgentEvent::TextDelta { text: "x".into() },
            task_id,
            Some(name),
        ));
    }
    finish_subagent(&mut app, FINISHED_TASK_ID, false);
    assert_eq!(app.state.session.subagents().len(), 2);

    app.update(done_event());
    assert!(app.chat_index.is_empty());
    app.checkpoint();

    let ids: Vec<_> = app
        .state
        .session
        .subagents()
        .iter()
        .map(|sa| sa.tool_use_id.as_str())
        .collect();
    assert_eq!(ids, [FINISHED_TASK_ID]);
}

#[test]
fn run_builtin_file_picker_opens_modal() {
    let mut app = test_app();
    assert!(app.run_builtin(BuiltinAction::FilePicker).is_empty());
    assert!(app.file_picker.is_open());
}

#[test]
fn run_builtin_model_picker_opens_and_refreshes() {
    let mut app = test_app();
    let actions = app.run_builtin(BuiltinAction::ModelPicker);
    assert!(app.model_picker.is_open());
    assert!(matches!(&actions[..], [Action::RefreshModels]));
}

#[test]
fn alt_m_opens_model_picker() {
    let mut app = test_app();
    let key = KeyEvent {
        code: KeyCode::Char('m'),
        modifiers: KeyModifiers::CONTROL,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    app.update(Msg::Key(key));
    assert!(app.model_picker.is_open());
}
